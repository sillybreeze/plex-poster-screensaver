//! 窗口、事件循环与动效调度（D3D11 渲染）。

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Fullscreen, Window, WindowId};

use crate::config::Config;
use crate::gpu::{Gpu, Textures};
use crate::plex::{Cache, Client, MediaItem, Section};
use crate::render::{self, Composition, DIRECTIONS};

pub struct Slide {
    pub item: MediaItem,
    pub comp: Composition,
}

/// 按库轮询、分批补货，尽量不连续出现同一张。
pub struct Source {
    client: Client,
    cache: Cache,
    sections: Vec<Section>,
    size: (u32, u32),
    buffers: HashMap<String, VecDeque<MediaItem>>,
    recent: VecDeque<String>,
    rr: usize,
    batch: usize,
}

impl Source {
    pub fn new(client: Client, cache: Cache, sections: Vec<Section>, size: (u32, u32)) -> Self {
        let buffers = sections
            .iter()
            .map(|s| (s.key.clone(), VecDeque::new()))
            .collect();
        Self {
            client,
            cache,
            sections,
            size,
            buffers,
            recent: VecDeque::new(),
            rr: 0,
            batch: 40,
        }
    }

    fn refill(&mut self, key: &str) -> Result<(), String> {
        let items = self.client.random_items(key, self.batch)?;
        if let Some(buf) = self.buffers.get_mut(key) {
            buf.extend(items);
        }
        Ok(())
    }

    pub fn next(&mut self) -> Result<Slide, String> {
        for attempt in 0..300 {
            let section = &self.sections[self.rr % self.sections.len()];
            self.rr = self.rr.wrapping_add(1);
            let key = section.key.clone();

            let empty = self.buffers.get(&key).map(|b| b.is_empty()).unwrap_or(true);
            if empty {
                self.refill(&key)?;
            }
            let Some(item) = self.buffers.get_mut(&key).and_then(|b| b.pop_front()) else {
                continue;
            };

            if self.recent.contains(&item.rating_key) && attempt < 60 {
                continue;
            }

            let comp = match self.cache.get(&item) {
                Some(path) => render::build(&path, self.size.0, self.size.1)?,
                None => {
                    let data = self.client.fetch_cover(&item, self.size.0, self.size.1)?;
                    if data.is_empty() {
                        return Err(format!("封面内容为空: {}", item.title));
                    }
                    match self.cache.store(&item, &data) {
                        Some(path) => render::build(&path, self.size.0, self.size.1)?,
                        None => {
                            render::build_from_bytes(&data, self.size.0, self.size.1)?
                        }
                    }
                }
            };

            self.recent.push_back(item.rating_key.clone());
            if self.recent.len() > 12 {
                self.recent.pop_front();
            }
            return Ok(Slide { item, comp });
        }
        Err("连续多次都没能取到封面".to_string())
    }
}

enum Phase {
    Idle,
    Fade {
        old: Textures,
        old_uv: [f32; 4],
        start: Instant,
    },
    Motion {
        start: Instant,
    },
}

pub struct App {
    cfg: Config,
    preview: bool,
    pending: Option<(Client, Cache, Vec<Section>)>,
    rx: Option<Receiver<Slide>>,

    size: (u32, u32),
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,

    current: Option<Textures>,
    phase: Phase,
    slide_index: usize,
    progress: f32,
    uv: [f32; 4],

    next_frame_at: Instant,
    grace_until: Instant,
    pointer_start: (f64, f64),
    started: bool,
    frames: u64,
    last_report: Instant,
    tick_us: f64,
    present_us: f64,
}

impl App {
    pub fn new(
        cfg: Config,
        preview: bool,
        client: Client,
        cache: Cache,
        sections: Vec<Section>,
    ) -> Self {
        let now = Instant::now();
        Self {
            cfg,
            preview,
            pending: Some((client, cache, sections)),
            rx: None,
            size: (1920, 1080),
            window: None,
            gpu: None,
            current: None,
            phase: Phase::Idle,
            slide_index: 0,
            progress: 0.0,
            uv: [0.0, 0.0, 1.0, 1.0],
            next_frame_at: now,
            grace_until: now + Duration::from_millis(1500),
            pointer_start: (0.0, 0.0),
            started: false,
            frames: 0,
            last_report: now,
            tick_us: 0.0,
            present_us: 0.0,
        }
    }

    pub fn run(mut self) -> Result<(), String> {
        let event_loop = EventLoop::new().map_err(|e| format!("创建事件循环失败: {e}"))?;
        event_loop
            .run_app(&mut self)
            .map_err(|e| format!("事件循环异常: {e}"))
    }

    fn start_worker(&mut self) {
        let Some((client, cache, sections)) = self.pending.take() else {
            return;
        };
        let size = self.size;
        let (tx, rx): (SyncSender<Slide>, Receiver<Slide>) = sync_channel(1);
        self.rx = Some(rx);
        thread::spawn(move || {
            let mut source = Source::new(client, cache, sections, size);
            loop {
                match source.next() {
                    Ok(slide) => {
                        if tx.send(slide).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        eprintln!("取图失败: {err}");
                        thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        });
    }

    fn direction(&self) -> (f32, f32) {
        DIRECTIONS[self.slide_index % DIRECTIONS.len()]
    }

    fn take_next(&mut self, now: Instant) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        let Ok(slide) = rx.try_recv() else {
            return false;
        };

        eprintln!("显示: {}", slide.item.title);
        self.slide_index += 1;

        let Some(gpu) = &self.gpu else {
            return false;
        };
        let new_tex =
            match gpu.upload(&slide.comp.background, &slide.comp.poster, slide.comp.position) {
                Ok(tex) => tex,
                Err(err) => {
                    eprintln!("上传纹理失败: {err}");
                    return false;
                }
            };

        match self.current.take() {
            Some(old) => {
                self.current = Some(new_tex);
                self.phase = Phase::Fade {
                    old,
                    old_uv: self.uv,
                    start: now,
                };
            }
            None => {
                self.current = Some(new_tex);
                self.phase = Phase::Motion { start: now };
                self.progress = 0.0;
                self.uv = [0.0, 0.0, 1.0, 1.0];
            }
        }
        self.next_frame_at = now;
        true
    }

    fn tick(&mut self, now: Instant) {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        match phase {
            Phase::Idle => {
                if self.take_next(now) {
                    return;
                }
                self.next_frame_at = now + Duration::from_millis(120);
            }
            Phase::Fade {
                old,
                old_uv,
                start,
            } => {
                let duration = self.cfg.fade_seconds.max(0.01);
                let t = ((now - start).as_secs_f64() / duration) as f32;
                if t >= 1.0 {
                    self.begin_motion(now);
                } else {
                    let uv = self.uv_at(0.0);
                    if let (Some(gpu), Some(current)) = (&self.gpu, &self.current) {
                        // 旧图不透明打底，新图按 alpha 叠加 = 标准交叉淡化
                        gpu.draw(&old, old_uv, 1.0);
                        gpu.draw(current, uv, t * t * (3.0 - 2.0 * t));
                    }
                    self.phase = Phase::Fade {
                        old,
                        old_uv,
                        start,
                    };
                    self.next_frame_at = now + Duration::from_millis(33);
                }
            }
            Phase::Motion { start } => {
                self.phase = Phase::Motion { start };
                self.render_motion(now, start);
            }
        }
    }

    fn begin_motion(&mut self, now: Instant) {
        self.phase = Phase::Motion { start: now };
        self.progress = 0.0;
        self.next_frame_at = now;
    }

    fn uv_at(&self, progress: f32) -> [f32; 4] {
        match &self.current {
            Some(tex) => {
                let rect = tex.rect();
                render::ken_burns_uv(rect[2], rect[3], progress, self.direction())
            }
            None => [0.0, 0.0, 1.0, 1.0],
        }
    }

    fn render_motion(&mut self, now: Instant, start: Instant) {
        let display = self.cfg.display_seconds.max(0.1);
        let progress = if self.cfg.ken_burns {
            ((now - start).as_secs_f64() / display).min(1.0) as f32
        } else {
            0.0
        };
        self.progress = progress;
        self.uv = self.uv_at(progress);

        if let (Some(gpu), Some(current)) = (&self.gpu, &self.current) {
            gpu.draw(current, self.uv, 1.0);
        }

        if progress >= 1.0 {
            if self.take_next(now) {
                return;
            }
            self.next_frame_at = now + Duration::from_millis(120);
        } else if self.cfg.ken_burns {
            let fps = self.cfg.motion_fps.max(1);
            self.next_frame_at = now + Duration::from_millis(1000 / fps as u64);
        } else {
            self.next_frame_at = now + Duration::from_millis(120);
        }
    }

    fn present(&mut self) {
        if let Some(gpu) = &self.gpu {
            gpu.present();
        }
    }

    fn report(&mut self, now: Instant) {
        if std::env::var_os("PPS_DEBUG").is_none() {
            return;
        }
        let elapsed = now.duration_since(self.last_report).as_secs_f64();
        if elapsed < 1.0 {
            return;
        }
        let n = self.frames as f64;
        eprintln!(
            "fps {:.1} | tick {:.2} ms | present {:.2} ms | progress {:.2}",
            n / elapsed,
            self.tick_us / n,
            self.present_us / n,
            self.progress
        );
        self.frames = 0;
        self.tick_us = 0.0;
        self.present_us = 0.0;
        self.last_report = now;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let mut attrs = Window::default_attributes().with_title("Plex 海报屏保");
        if self.preview {
            attrs = attrs.with_inner_size(LogicalSize::new(1280.0, 720.0));
        } else {
            attrs = attrs
                .with_decorations(false)
                .with_fullscreen(Some(Fullscreen::Borderless(None)));
        }

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(err) => {
                eprintln!("创建窗口失败: {err}");
                event_loop.exit();
                return;
            }
        };

        let size = window.inner_size();
        self.size = (size.width.max(1), size.height.max(1));

        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => {
                windows::Win32::Foundation::HWND(h.hwnd.get() as *mut core::ffi::c_void)
            }
            _ => {
                eprintln!("无法获取窗口句柄");
                event_loop.exit();
                return;
            }
        };

        match Gpu::new(hwnd, self.size.0, self.size.1) {
            Ok(gpu) => self.gpu = Some(gpu),
            Err(err) => {
                eprintln!("初始化 D3D11 失败: {err}");
                event_loop.exit();
                return;
            }
        }

        self.window = Some(window);

        if !self.started {
            self.started = true;
            self.grace_until = Instant::now() + Duration::from_millis(1500);
            self.start_worker();
        }
        self.next_frame_at = Instant::now();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed && Instant::now() >= self.grace_until {
                    event_loop.exit();
                }
            }
            WindowEvent::MouseInput { state, .. } => {
                if state == ElementState::Pressed && Instant::now() >= self.grace_until {
                    event_loop.exit();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let now = Instant::now();
                if now < self.grace_until {
                    self.pointer_start = (position.x, position.y);
                } else if !self.preview {
                    let dx = position.x - self.pointer_start.0;
                    let dy = position.y - self.pointer_start.1;
                    if dx * dx + dy * dy > 1600.0 {
                        event_loop.exit();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.window.is_some() && now >= self.next_frame_at {
            let t0 = Instant::now();
            self.tick(now);
            let t1 = Instant::now();
            self.present();
            let t2 = Instant::now();
            self.frames += 1;
            self.tick_us += (t1 - t0).as_secs_f64() * 1000.0;
            self.present_us += (t2 - t1).as_secs_f64() * 1000.0;
            self.report(t2);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame_at));
    }
}

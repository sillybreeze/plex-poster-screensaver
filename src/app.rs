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
use crate::idle::{self, Idle};
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

/// 常驻等待状态：窗口先藏着，空闲到点才全屏。
struct Watch {
    threshold: Duration,
    idle: Idle,
    showing: bool,
    next_poll: Instant,
    /// 显示那一刻的系统输入时间戳；变了说明有人动了鼠标/键盘
    dismiss_tick: u32,
}

pub struct App {
    cfg: Config,
    preview: bool,
    watch: Option<Watch>,
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
        let watch = if cfg.watch && !preview {
            Some(Watch {
                threshold: Duration::from_secs_f64(cfg.idle_seconds.max(1.0)),
                idle: Idle::new(cfg.mouse_only, now),
                showing: false,
                next_poll: now,
                dismiss_tick: 0,
            })
        } else {
            None
        };
        Self {
            cfg,
            preview,
            watch,
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
        if let Some(w) = &self.watch {
            println!(
                "常驻等待：空闲 {:.0} 秒后自动全屏，动鼠标/按键就收起",
                w.threshold.as_secs_f64()
            );
        }
        let event_loop = EventLoop::new().map_err(|e| format!("创建事件循环失败: {e}"))?;
        event_loop
            .run_app(&mut self)
            .map_err(|e| format!("事件循环异常: {e}"))
    }

    /// 惰性创建 D3D11：常驻模式下第一次触发才建，平时不占显存。
    fn ensure_gpu(&mut self) -> bool {
        if self.gpu.is_some() {
            return true;
        }
        let Some(window) = self.window.clone() else {
            return false;
        };
        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => {
                windows::Win32::Foundation::HWND(h.hwnd.get() as *mut core::ffi::c_void)
            }
            _ => {
                eprintln!("无法获取窗口句柄");
                return false;
            }
        };
        match Gpu::new(hwnd, self.size.0, self.size.1) {
            Ok(gpu) => {
                self.gpu = Some(gpu);
                true
            }
            Err(err) => {
                eprintln!("初始化 D3D11 失败: {err}");
                false
            }
        }
    }

    /// 空闲到点：把隐藏的窗口全屏显示出来。
    fn show_screensaver(&mut self, event_loop: &ActiveEventLoop, now: Instant) {
        let Some(window) = self.window.clone() else {
            return;
        };

        if let Some(monitor) = event_loop.primary_monitor() {
            let size = monitor.size();
            self.size = (size.width.max(1), size.height.max(1));
        }
        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        window.set_visible(true);
        window.focus_window();

        if !self.ensure_gpu() {
            // 建不出渲染器就别干挂着黑屏，一分钟后再试
            window.set_fullscreen(None);
            window.set_visible(false);
            if let Some(w) = &mut self.watch {
                w.idle.reset(now);
                w.next_poll = now + Duration::from_secs(60);
            }
            return;
        }

        self.current = None;
        self.phase = Phase::Idle;
        self.progress = 0.0;
        self.uv = [0.0, 0.0, 1.0, 1.0];
        self.next_frame_at = now;
        self.grace_until = now + Duration::from_millis(700);

        if let Some(w) = &mut self.watch {
            w.showing = true;
            w.dismiss_tick = idle::last_input_tick();
        }
        eprintln!("空闲 {:.0} 秒，开始屏保", self.cfg.idle_seconds);
    }

    /// 有人动了：收起屏保，回到常驻等待。
    fn hide_screensaver(&mut self, now: Instant) {
        eprintln!("检测到输入，收起屏保");
        if let Some(window) = &self.window {
            window.set_fullscreen(None);
            window.set_visible(false);
        }
        if let Some(w) = &mut self.watch {
            w.showing = false;
            w.idle.reset(now);
            w.next_poll = now + Duration::from_millis(500);
        }
        self.current = None;
        self.phase = Phase::Idle;
        self.progress = 0.0;
        self.next_frame_at = now;
    }

    /// 窗口收到输入。常驻模式收起，普通模式直接退出。
    fn user_activity(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if now < self.grace_until {
            return;
        }
        let showing = self.watch.as_ref().map(|w| w.showing).unwrap_or(false);
        if self.watch.is_some() {
            if showing {
                self.hide_screensaver(now);
            }
            return;
        }
        event_loop.exit();
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

        let watching = self.watch.is_some();
        let mut attrs = Window::default_attributes().with_title("Plex 海报屏保");
        if self.preview {
            attrs = attrs.with_inner_size(LogicalSize::new(1280.0, 720.0));
        } else if watching {
            // 常驻模式：先建一个隐藏窗口，空闲到点再全屏显示
            attrs = attrs.with_visible(false).with_decorations(false);
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

        // 隐藏窗口的尺寸没意义，直接按主屏来 —— 工作线程要按目标分辨率合成海报
        let size = if watching {
            event_loop
                .primary_monitor()
                .map(|m| m.size())
                .unwrap_or_else(|| window.inner_size())
        } else {
            window.inner_size()
        };
        self.size = (size.width.max(1), size.height.max(1));

        self.window = Some(window);

        if !watching && !self.ensure_gpu() {
            event_loop.exit();
            return;
        }

        if !self.started {
            self.started = true;
            self.start_worker();
        }
        let now = Instant::now();
        self.grace_until = now + Duration::from_millis(1500);
        self.next_frame_at = now;
        if let Some(w) = &mut self.watch {
            w.next_poll = now;
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    self.user_activity(event_loop);
                }
            }
            WindowEvent::MouseInput { state, .. } => {
                if state == ElementState::Pressed {
                    self.user_activity(event_loop);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let now = Instant::now();
                if now < self.grace_until {
                    self.pointer_start = (position.x, position.y);
                } else if self.watch.is_some() {
                    // 常驻模式：鼠标一动就收起（藏着的窗口不该收到这个事件，防一手）
                    if self.watch.as_ref().is_some_and(|w| w.showing) {
                        self.hide_screensaver(now);
                    }
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

        // 常驻模式：藏着的时候数空闲，显示的时候盯输入
        let mut start = false;
        let mut stop = false;
        if let Some(w) = self.watch.as_mut() {
            if w.showing {
                if now >= self.grace_until && idle::last_input_tick() != w.dismiss_tick {
                    stop = true;
                }
            } else if now >= w.next_poll {
                w.next_poll = now + Duration::from_millis(500);
                let idle = w.idle.poll(now);
                if std::env::var_os("PPS_DEBUG").is_some() {
                    eprintln!(
                        "[watch] 已空闲 {:.1}s / 阈值 {:.0}s",
                        idle.as_secs_f64(),
                        w.threshold.as_secs_f64()
                    );
                }
                if idle >= w.threshold {
                    start = true;
                }
            }
        }
        if start {
            self.show_screensaver(event_loop, now);
        }
        if stop {
            self.hide_screensaver(now);
        }
        if let Some(w) = &self.watch {
            if !w.showing {
                event_loop.set_control_flow(ControlFlow::WaitUntil(w.next_poll));
                return;
            }
        }

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

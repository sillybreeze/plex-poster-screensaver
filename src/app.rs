//! 窗口、事件循环与动效调度（D3D11 渲染）。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::windows::WindowExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Fullscreen, Window, WindowId};

use crate::config::Config;
use crate::gpu::{Gpu, GpuError, PresentStatus, Textures};
use crate::idle::{self, Idle};
use crate::logging::{log_error, log_info, log_warn, Throttle};
use crate::platform::Platform;
use crate::plex::{Cache, Client, MediaItem, Section};
use crate::render::{self, Composition, DIRECTIONS};
use crate::status::{self, Shared};

pub struct Slide {
    pub item: MediaItem,
    pub comp: Composition,
}

/// 断网时每隔多久重试一次 Plex。
const RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

/// 按库轮询、分批补货，尽量不连续出现同一张；断网时退到本地索引播放。
pub struct Source {
    client: Client,
    cache: Cache,
    sections: Vec<Section>,
    size: (u32, u32),
    buffers: HashMap<String, VecDeque<MediaItem>>,
    recent: VecDeque<String>,
    rr: usize,
    batch: usize,
    offline: VecDeque<MediaItem>,
    online: bool,
    next_reconnect: Instant,
    /// 用户配置的媒体库筛选；重连时仍然要按它选，不能把没选的库也播出来。
    wanted: Vec<String>,
    status: Shared,
}

impl Source {
    pub fn new(
        client: Client,
        cache: Cache,
        sections: Vec<Section>,
        size: (u32, u32),
        wanted: Vec<String>,
        status: Shared,
    ) -> Self {
        let buffers = sections
            .iter()
            .map(|s| (s.key.clone(), VecDeque::new()))
            .collect();
        let online = !sections.is_empty();
        status::update(&status, |current| {
            current.online = online;
            if online {
                current.last_error.clear();
            }
        });
        let now = Instant::now();
        let mut source = Self {
            client,
            cache,
            sections,
            size,
            buffers,
            recent: VecDeque::new(),
            rr: 0,
            batch: 40,
            offline: VecDeque::new(),
            online,
            // 启动时已经试过一次连接；离线就先放缓存，30 秒后再重连。
            next_reconnect: if online {
                now
            } else {
                now + RECONNECT_INTERVAL
            },
            wanted,
            status,
        };
        source.reload_offline();
        source
    }

    fn refill(&mut self, section: &Section) -> Result<usize, String> {
        let items = self.client.random_items(section, self.batch)?;
        let count = items.len();
        if let Some(buf) = self.buffers.get_mut(&section.key) {
            buf.extend(items);
        }
        Ok(count)
    }

    /// 从磁盘索引重新装填离线队列，并打乱顺序，避免每次都是同样几张。
    fn reload_offline(&mut self) {
        let mut items = self.cache.offline_items(&self.wanted);
        shuffle(&mut items);
        self.offline = items.into();
    }

    fn go_offline(&mut self) {
        if self.online {
            log_warn!("Plex 暂时取不到图，改用本地缓存离线播放");
        }
        self.online = false;
        self.next_reconnect = Instant::now() + RECONNECT_INTERVAL;
        status::update(&self.status, |current| {
            current.online = false;
            if current.last_error.is_empty() {
                current.last_error = "Plex 连接不可用，使用本地缓存".to_string();
            }
        });
        self.reload_offline();
    }

    fn try_reconnect(&mut self, now: Instant) {
        self.next_reconnect = now + RECONNECT_INTERVAL;
        match self.client.sections() {
            Ok(sections) if !sections.is_empty() => {
                // 记下完整的库列表，离线时才能把 --library 的编号解析回库。
                self.cache.remember_sections(&sections);
                // 重连后仍按 --library/config.libraries 过滤，不能把用户没选的库也播出来。
                let chosen = crate::plex::filter_sections(&sections, &self.wanted);
                if chosen.is_empty() {
                    log_warn!("Plex 已恢复，但配置的媒体库一个都没匹配到");
                    return;
                }
                log_info!("已重新连接 Plex：{} 个媒体库", chosen.len());
                self.sections = chosen;
                self.buffers = self
                    .sections
                    .iter()
                    .map(|s| (s.key.clone(), VecDeque::new()))
                    .collect();
                self.rr = 0;
                self.online = true;
                status::update(&self.status, |current| {
                    current.online = true;
                    current.last_error.clear();
                });
            }
            Ok(_) => {}
            Err(err) => {
                log_warn!("重连 Plex 失败: {err}");
                status::update(&self.status, |current| {
                    current.last_error = format!("重连 Plex 失败: {err}");
                });
            }
        }
    }

    fn remember(&mut self, key: &str) {
        self.recent.push_back(key.to_string());
        if self.recent.len() > 12 {
            self.recent.pop_front();
        }
    }

    fn finish(&mut self, item: MediaItem, comp: Composition) -> Slide {
        self.remember(&item.rating_key);
        Slide { item, comp }
    }

    /// 在线取一张。只有连续多次取图失败才切离线，单个坏条目不至于让整个屏保定格。
    fn online_next(&mut self) -> Result<Option<Slide>, String> {
        if self.sections.is_empty() {
            self.go_offline();
            return Ok(None);
        }

        let mut errors = 0;
        let mut empty_rounds = 0;
        for attempt in 0..300 {
            let section = self.sections[self.rr % self.sections.len()].clone();
            self.rr = self.rr.wrapping_add(1);
            let key = section.key.clone();

            let empty = self.buffers.get(&key).map(|b| b.is_empty()).unwrap_or(true);
            if empty {
                match self.refill(&section) {
                    Ok(0) => {
                        // 这个库是空的；所有库都空就别再刷请求了。
                        empty_rounds += 1;
                        if empty_rounds >= self.sections.len() {
                            log_warn!("Plex 媒体库里没有条目，改用本地缓存");
                            self.go_offline();
                            return Ok(None);
                        }
                        continue;
                    }
                    Ok(_) => empty_rounds = 0,
                    Err(err) => {
                        log_warn!("补充取图列表失败: {err}");
                        status::update(&self.status, |current| {
                            current.last_error = format!("补充取图列表失败: {err}");
                        });
                        self.go_offline();
                        return Ok(None);
                    }
                }
            }

            // 这一批里如果已经没有「最近没放过」的条目，就直接允许重复。
            // 小库否则会为了一张图反复请求随机列表。
            let has_fresh = self
                .buffers
                .get(&key)
                .map(|b| b.iter().any(|i| !self.recent.contains(&i.rating_key)))
                .unwrap_or(false);

            let Some(item) = self.buffers.get_mut(&key).and_then(|b| b.pop_front()) else {
                continue;
            };

            if has_fresh && self.recent.contains(&item.rating_key) && attempt < 60 {
                continue;
            }

            match self.slide_for(&item) {
                Ok(comp) => return Ok(Some(self.finish(item, comp))),
                Err(err) => {
                    log_warn!("取图失败 {}: {err}", item.title);
                    status::update(&self.status, |current| {
                        current.last_error = format!("取图失败: {err}");
                    });
                    errors += 1;
                    if errors >= 3 {
                        self.go_offline();
                        return Ok(None);
                    }
                }
            }
        }
        Ok(None)
    }

    /// 在线路径：缓存命中直接解码；命中坏缓存则删掉重下；没有缓存则下载。
    fn slide_for(&mut self, item: &MediaItem) -> Result<Composition, String> {
        if let Some(path) = self.cache.get(item) {
            match render::build(&path, self.size.0, self.size.1) {
                Ok(comp) => {
                    self.cache.remember(item);
                    return Ok(comp);
                }
                Err(err) => {
                    log_warn!("缓存损坏，删除后重新下载: {err}");
                    self.cache.discard(&path);
                }
            }
        }

        let data = self.client.fetch_cover(item, self.size.0, self.size.1)?;
        if data.is_empty() {
            return Err(format!("封面内容为空: {}", item.title));
        }
        // 先确认能解码再落盘，避免把坏内容存成 .jpg 后一直命中坏缓存。
        let comp = render::build_from_bytes(&data, self.size.0, self.size.1)?;
        self.cache.store(item, &data);
        Ok(comp)
    }

    /// 离线路径：只读缓存，绝不访问网络。
    fn cached_slide(&mut self, item: &MediaItem) -> Result<Composition, String> {
        let path = self
            .cache
            .get(item)
            .ok_or_else(|| format!("缓存文件不存在: {}", item.title))?;
        match render::build(&path, self.size.0, self.size.1) {
            Ok(comp) => {
                self.cache.remember(item);
                Ok(comp)
            }
            Err(err) => {
                self.cache.discard(&path);
                Err(err)
            }
        }
    }

    fn offline_next(&mut self) -> Result<Slide, String> {
        if self.offline.is_empty() {
            self.reload_offline();
        }
        let count = self.offline.len();
        for _ in 0..count {
            let Some(item) = self.offline.pop_front() else {
                break;
            };
            match self.cached_slide(&item) {
                Ok(comp) => return Ok(self.finish(item, comp)),
                Err(err) => log_warn!("离线缓存不可用 {}: {err}", item.title),
            }
        }
        let message = "无法连接 Plex，本地缓存也没有可用封面".to_string();
        status::update(&self.status, |current| {
            current.last_error = message.clone();
        });
        Err(message)
    }

    pub fn next(&mut self) -> Result<Slide, String> {
        let now = Instant::now();
        self.cache.maintain_if_due();
        if !self.online && now >= self.next_reconnect {
            self.try_reconnect(now);
        }
        if self.online {
            match self.online_next() {
                Ok(Some(slide)) => {
                    status::update(&self.status, |current| {
                        current.online = true;
                        current.last_error.clear();
                    });
                    return Ok(slide);
                }
                Ok(None) => {}
                Err(err) => {
                    log_warn!("在线取图失败: {err}");
                    status::update(&self.status, |current| {
                        current.last_error = err.clone();
                    });
                    self.go_offline();
                }
            }
        }
        self.offline_next()
    }
}

/// 无 rand 依赖的 Fisher-Yates；只用于打乱本地缓存播放顺序。
fn shuffle(items: &mut [MediaItem]) {
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    for i in (1..items.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = ((state >> 33) as usize) % (i + 1);
        items.swap(i, j);
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

/// 窗口尺寸连续变化时的稳定等待；拖动预览窗口时别反复重建交换链和预取线程。
const RESIZE_SETTLE: Duration = Duration::from_millis(250);
/// 空闲到点时刚显示的那一小段：别被残留的鼠标抖动立刻关掉。
const IDLE_GRACE: Duration = Duration::from_millis(700);
/// 手动拉起（热键 / 再启动）留得久一点：按下和松开热键本身都算输入。
const MANUAL_GRACE: Duration = Duration::from_millis(1200);
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn window_icon() -> Option<winit::window::Icon> {
    let rgba = image::load_from_memory(WINDOW_ICON_PNG).ok()?.to_rgba8();
    let (width, height) = rgba.dimensions();
    winit::window::Icon::from_rgba(rgba.into_raw(), width, height).ok()
}

/// 事件循环的自定义事件：托盘、系统会话/电源通知和再启动都会走这里。
pub enum UserEvent {
    StartNow,
    PauseFor(Duration),
    ResumeAuto,
    Quit,
    SessionLocked,
    SessionUnlocked,
    PowerSuspend,
    PowerResume,
    DisplayOff,
    DisplayOn,
}

/// 暂停自动触发的各种原因。手动暂停是有期限的，系统原因由通知开关。
#[derive(Default)]
struct PauseState {
    manual_until: Option<Instant>,
    locked: bool,
    suspended: bool,
    display_off: bool,
    dnd: bool,
}

impl PauseState {
    fn is_paused(&self, now: Instant) -> bool {
        self.manual_until.is_some_and(|until| now < until)
            || self.locked
            || self.suspended
            || self.display_off
            || self.dnd
    }

    fn system_paused(&self) -> bool {
        self.locked || self.suspended || self.display_off
    }

    fn reason(&self, now: Instant) -> &'static str {
        if self.manual_until.is_some_and(|until| now < until) {
            "手动暂停"
        } else if self.locked {
            "锁屏"
        } else if self.suspended {
            "休眠"
        } else if self.display_off {
            "显示器关闭"
        } else if self.dnd {
            "全屏应用/演示"
        } else {
            ""
        }
    }

    fn clear_expired(&mut self, now: Instant) {
        if self.manual_until.is_some_and(|until| now >= until) {
            self.manual_until = None;
            log_info!("手动暂停到期，恢复自动触发");
        }
    }
}

/// GPU 故障日志最多每 30 秒汇总一次。
const GPU_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// 设备创建、Present、resize 失败后的统一恢复调度。
///
/// 失败次数不会在“设备创建成功”时清零：只有新设备确实成功呈现过一次才算恢复，
/// 否则会退化成“创建成功 → Present 失败 → 立即重建”的忙循环。
#[derive(Default)]
struct GpuRecovery {
    failures: u32,
    retry_at: Option<Instant>,
    recovering: bool,
    log: Throttle,
}

/// 1、2、4、8、16、32、60 秒，上限 60 秒。
fn gpu_backoff(failures: u32) -> Duration {
    match failures {
        0 | 1 => Duration::from_secs(1),
        2 => Duration::from_secs(2),
        3 => Duration::from_secs(4),
        4 => Duration::from_secs(8),
        5 => Duration::from_secs(16),
        6 => Duration::from_secs(32),
        _ => Duration::from_secs(60),
    }
}

impl GpuRecovery {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn retry_at(&self) -> Option<Instant> {
        self.retry_at
    }

    fn retry_in(&self, now: Instant) -> Duration {
        self.retry_at
            .map(|at| at.saturating_duration_since(now))
            .unwrap_or(Duration::ZERO)
    }

    fn can_attempt(&self, now: Instant) -> bool {
        self.retry_at.map(|at| now >= at).unwrap_or(true)
    }

    /// 记录一次失败并推进退避；`Some(n)` 表示本次应写日志，`n` 是被抑制的次数。
    fn record_failure(&mut self, now: Instant) -> Option<u32> {
        self.failures = self.failures.saturating_add(1);
        self.recovering = true;
        self.retry_at = Some(now + gpu_backoff(self.failures));
        self.log.record(now, GPU_LOG_INTERVAL)
    }

    /// 一次成功呈现后恢复；返回此前是否处于故障恢复中。
    fn record_success(&mut self) -> bool {
        let recovering = self.recovering;
        self.reset();
        recovering
    }
}

/// 常驻等待状态：窗口先藏着，空闲到点才全屏。
struct Watch {
    threshold: Duration,
    idle: Idle,
    showing: bool,
    /// 是不是热键/再启动这类「手动」拉起的（触发的那个动作本身就是输入）
    manual: bool,
    next_poll: Instant,
    /// 显示那一刻的系统输入时间戳；变了说明有人动了鼠标/键盘。
    /// 检测失败时为 `None`，恢复后重新取基准，不拿「不知道」当「有人回来」。
    dismiss_tick: Option<u32>,
    /// 上一轮空闲检测是否失败；用来只告警一次。
    idle_failed: bool,
    /// 已经空闲到点但还没有内容可显示；别把 idle 重置掉，等有内容后立刻触发。
    waiting_content: bool,
}

pub struct App {
    cfg: Config,
    preview: bool,
    watch: Option<Watch>,
    client: Client,
    cache: Cache,
    sections: Vec<Section>,
    wanted: Vec<String>,
    rx: Option<Receiver<Slide>>,
    /// 常驻隐藏时先从通道里取出来的一张，避免空闲到点却还没内容可显示。
    pending_slide: Option<Slide>,
    /// 当前预取线程的取消标志；重建/关闭时置位，失败重试路径也要能退出。
    worker_cancel: Option<Arc<AtomicBool>>,

    status: Shared,
    config_path: Option<PathBuf>,
    platform: Option<Platform>,
    pause: PauseState,
    next_dnd_check: Instant,

    size: (u32, u32),
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    gpu_recovery: GpuRecovery,
    /// 非设备类 Present 错误的日志限流，避免瞬时错误刷屏。
    transient_log: Throttle,

    current: Option<Textures>,
    phase: Phase,
    slide_index: usize,
    progress: f32,
    uv: [f32; 4],

    next_frame_at: Instant,
    grace_until: Instant,
    pointer_start: (f64, f64),
    /// 窗口连续变化时先等它稳定，再重建交换链和预取线程。
    pending_resize: Option<(Instant, (u32, u32))>,
    frames: u64,
    last_report: Instant,
    tick_us: f64,
    present_us: f64,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        preview: bool,
        client: Client,
        cache: Cache,
        sections: Vec<Section>,
        wanted: Vec<String>,
        status: Shared,
        config_path: Option<PathBuf>,
    ) -> Self {
        let now = Instant::now();
        let watch = if cfg.watch && !preview {
            Some(Watch {
                threshold: Duration::from_secs_f64(cfg.idle_seconds.max(1.0)),
                idle: Idle::new(cfg.mouse_only, now),
                showing: false,
                manual: false,
                next_poll: now,
                dismiss_tick: None,
                idle_failed: false,
                waiting_content: false,
            })
        } else {
            None
        };
        Self {
            cfg,
            preview,
            watch,
            client,
            cache,
            sections,
            wanted,
            rx: None,
            pending_slide: None,
            worker_cancel: None,
            status,
            config_path,
            platform: None,
            pause: PauseState::default(),
            next_dnd_check: now,
            size: (1920, 1080),
            window: None,
            gpu: None,
            gpu_recovery: GpuRecovery::default(),
            transient_log: Throttle::default(),
            current: None,
            phase: Phase::Idle,
            slide_index: 0,
            progress: 0.0,
            uv: [0.0, 0.0, 1.0, 1.0],
            next_frame_at: now,
            grace_until: now + Duration::from_millis(1500),
            pointer_start: (0.0, 0.0),
            pending_resize: None,
            frames: 0,
            last_report: now,
            tick_us: 0.0,
            present_us: 0.0,
        }
    }

    pub fn run(mut self, trigger: Option<HANDLE>) -> Result<(), String> {
        if let Some(w) = &self.watch {
            log_info!(
                "常驻等待：空闲 {:.0} 秒后自动全屏，动鼠标/按键就收起",
                w.threshold.as_secs_f64()
            );
            match self.cfg.hotkey {
                Some(h) => log_info!("立刻开始：再启动一次 exe，或按 {}", h.label()),
                None => log_info!("立刻开始：再启动一次 exe"),
            }
        }
        let event_loop = EventLoop::<UserEvent>::with_user_event()
            .build()
            .map_err(|e| format!("创建事件循环失败: {e}"))?;

        if let Some(event) = trigger {
            let proxy = event_loop.create_proxy();
            crate::trigger::listen(event, move || {
                let _ = proxy.send_event(UserEvent::StartNow);
            });
        }

        // 热键只在常驻模式有意义：--now / 普通模式本来就在全屏
        if self.watch.is_some() {
            if let Some(hotkey) = self.cfg.hotkey {
                let proxy = event_loop.create_proxy();
                crate::trigger::listen_hotkey(hotkey, move || {
                    let _ = proxy.send_event(UserEvent::StartNow);
                });
            }
        }

        let tray_proxy = event_loop.create_proxy();
        match crate::platform::start(
            tray_proxy,
            self.status.clone(),
            self.config_path.clone(),
            self.cfg.cache_dir.clone(),
            crate::logging::log_dir(),
        ) {
            Ok(platform) => self.platform = Some(platform),
            Err(err) => log_warn!("托盘初始化失败，继续无托盘运行: {err}"),
        }

        let result = event_loop
            .run_app(&mut self)
            .map_err(|e| format!("事件循环异常: {e}"));
        self.platform = None;
        result
    }

    /// 惰性创建 D3D11：常驻模式下第一次触发才建，平时不占显存。
    ///
    /// 失败时进入统一退避，不在这里立即重试；隐藏或系统暂停期间也不创建。
    fn ensure_gpu(&mut self) -> bool {
        if self.gpu.is_some() {
            return true;
        }
        if self.pause.system_paused() {
            return false;
        }
        let now = Instant::now();
        if !self.gpu_recovery.can_attempt(now) {
            return false;
        }
        let Some(window) = self.window.clone() else {
            return false;
        };
        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => {
                windows::Win32::Foundation::HWND(h.hwnd.get() as *mut core::ffi::c_void)
            }
            _ => {
                log_error!("无法获取窗口句柄");
                return false;
            }
        };
        match Gpu::new(hwnd, self.size.0, self.size.1) {
            Ok(gpu) => {
                self.gpu = Some(gpu);
                true
            }
            Err(err) => {
                self.note_gpu_failure(now, &err);
                false
            }
        }
    }

    /// 记录一次 GPU 故障，按退避安排下次尝试并限制日志频率。
    fn note_gpu_failure(&mut self, now: Instant, err: &GpuError) {
        let Some(suppressed) = self.gpu_recovery.record_failure(now) else {
            return;
        };
        let retry = self.gpu_recovery.retry_in(now).as_secs().max(1);
        if suppressed == 0 {
            log_error!("{err}；{retry} 秒后重试");
        } else {
            log_error!("{err}；已抑制 {suppressed} 条同类日志；{retry} 秒后重试");
        }
    }

    /// 外部要求立刻开始（又启动了一次 exe）。只有常驻模式需要响应。
    fn start_now(&mut self, event_loop: &ActiveEventLoop) {
        match self.watch.as_ref().map(|w| w.showing) {
            None => return,        // --now 或普通模式，本来就在全屏
            Some(true) => return,  // 已经在放了
            Some(false) => {}
        }
        if self.pause.system_paused() {
            log_warn!("系统锁屏/休眠/显示器关闭中，忽略立即播放");
            return;
        }
        log_info!("收到启动请求，立刻开始");
        self.show_screensaver(event_loop, Instant::now(), true);
    }

    /// 空闲到点 / 手动要求：把隐藏的窗口全屏显示出来。
    ///
    /// `manual` 表示这次是热键或再启动拉起的 —— 按热键的那个动作本身就会更新系统的
    /// 「最后输入时间」，所以宽限期里要一直是刷新基准，否则松开按键就会被当成「有人回来了」。
    fn show_screensaver(&mut self, event_loop: &ActiveEventLoop, now: Instant, manual: bool) {
        let Some(window) = self.window.clone() else {
            return;
        };

        if !manual && self.pause.is_paused(now) {
            log_info!("自动触发被暂停（{}），忽略本次", self.pause.reason(now));
            return;
        }

        // 断网、缓存为空时先留在后台等 Plex，别全屏挂一块黑屏。
        self.poll_pending();
        if self.watch.is_some() && !self.has_content() {
            if let Some(w) = &mut self.watch {
                if !w.waiting_content {
                    w.waiting_content = true;
                    log_warn!("暂无可播放的海报，等待 Plex 恢复后再触发");
                }
                w.next_poll = now + Duration::from_secs(5);
            }
            return;
        }

        let monitor_size = event_loop.primary_monitor().map(|monitor| {
            let size = monitor.size();
            (size.width.max(1), size.height.max(1))
        });

        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        window.set_visible(true);
        window.focus_window();

        // 触发时才读主屏尺寸：如果和常驻启动时不同，交换链、合成尺寸、
        // 预取线程都要一起换掉，不能只更新 self.size 就继续用旧画面。
        self.pending_resize = None;
        if let Some(size) = monitor_size {
            self.update_size(size, now, true);
        }

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
        self.grace_until = now
            + if manual {
                MANUAL_GRACE
            } else {
                IDLE_GRACE
            };

        if let Some(w) = &mut self.watch {
            w.showing = true;
            w.manual = manual;
            w.waiting_content = false;
            w.dismiss_tick = idle::last_input_tick();
        }
    }

    /// 有人动了：收起屏保，回到常驻等待。
    fn hide_screensaver(&mut self, now: Instant) {
        log_info!("收起屏保");
        if let Some(window) = &self.window {
            window.set_fullscreen(None);
            window.set_visible(false);
        }
        if let Some(w) = &mut self.watch {
            w.showing = false;
            w.waiting_content = false;
            w.idle.reset(now);
            w.next_poll = now + Duration::from_millis(500);
        }
        self.current = None;
        self.phase = Phase::Idle;
        self.progress = 0.0;
        self.next_frame_at = now;
        self.pending_resize = None;
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

    /// 当前是否处于可见播放状态（常驻模式藏着的时候不算）。
    fn active(&self) -> bool {
        match &self.watch {
            Some(w) => w.showing,
            None => self.window.is_some(),
        }
    }

    /// 从预取通道里预读一张，隐藏等待时也就能知道「现在有没有内容可显示」。
    fn poll_pending(&mut self) {
        if self.pending_slide.is_some() {
            return;
        }
        let Some(rx) = &self.rx else {
            return;
        };
        if let Ok(slide) = rx.try_recv() {
            if (slide.comp.width, slide.comp.height) == self.size {
                self.pending_slide = Some(slide);
            }
        }
    }

    fn has_content(&self) -> bool {
        self.pending_slide.is_some() || self.current.is_some()
    }

    /// 停掉当前预取线程：置取消位、断开接收端，并丢弃待处理画面。
    ///
    /// 只靠 `send()` 失败发现接收端关闭是不够的：取图一直失败时线程永远走不到
    /// `send()`，反复重建尺寸会积累后台线程和重连请求。
    fn stop_worker(&mut self) {
        if let Some(cancel) = self.worker_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.rx = None;
        self.pending_slide = None;
    }

    /// 启动/重启预取线程。
    fn start_worker(&mut self) {
        self.stop_worker();
        let size = self.size;
        let (tx, rx): (SyncSender<Slide>, Receiver<Slide>) = sync_channel(1);
        self.rx = Some(rx);
        let cancel = Arc::new(AtomicBool::new(false));
        self.worker_cancel = Some(cancel.clone());

        let client = self.client.clone();
        let mut cache = self.cache.clone();
        // 工作线程自己会更新索引；重建时从磁盘读一次，别用启动时的旧副本。
        cache.reload_index();
        let sections = self.sections.clone();
        let wanted = self.wanted.clone();
        let status = self.status.clone();

        thread::spawn(move || {
            let mut source = Source::new(client, cache, sections, size, wanted, status);
            loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                match source.next() {
                    Ok(slide) => {
                        if cancel.load(Ordering::Relaxed) || tx.send(slide).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        log_warn!("取图失败: {err}");
                        // 3 秒分片睡眠，收到取消信号时立刻退出，不再无限重试。
                        for _ in 0..30 {
                            if cancel.load(Ordering::Relaxed) {
                                return;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            }
        });
    }

    /// 更新目标尺寸：能立即重建就立即重建；窗口连续拖动时先记下来，等稳定后再重建。
    fn update_size(&mut self, size: (u32, u32), now: Instant, immediate: bool) {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            if immediate && self.rx.is_none() {
                self.start_worker();
                self.next_frame_at = now;
            }
            return;
        }

        if std::env::var_os("PPS_DEBUG").is_some() {
            log_info!(
                "尺寸更新: {}x{} -> {}x{}（immediate={immediate}）",
                self.size.0, self.size.1, size.0, size.1
            );
        }
        self.size = size;

        // 旧纹理是按旧尺寸合成的，留着只会被拉伸；丢掉，等新尺寸的预取画面。
        self.current = None;
        self.phase = Phase::Idle;
        self.progress = 0.0;
        self.uv = [0.0, 0.0, 1.0, 1.0];

        let mut resize_error = None;
        if let Some(gpu) = &mut self.gpu {
            if gpu.size() != size {
                if let Err(err) = gpu.resize(size.0, size.1) {
                    resize_error = Some(err);
                }
            }
        }
        if let Some(err) = resize_error {
            // resize 失败同样走统一退避；不在事件处理里同步重建，交给 about_to_wait。
            self.gpu = None;
            self.current = None;
            self.phase = Phase::Idle;
            self.note_gpu_failure(now, &err);
        }

        // 预取队列里是旧尺寸的画面，立即作废；同时取消旧线程，别让它继续重试。
        self.stop_worker();

        if immediate {
            self.pending_resize = None;
            self.start_worker();
            self.next_frame_at = now;
        } else {
            self.pending_resize = Some((now, size));
            let due = now + RESIZE_SETTLE;
            if self.next_frame_at > due {
                self.next_frame_at = due;
            }
        }
    }

    /// 窗口拖拽/全屏切换会连续触发 Resized；等 250ms 没有新尺寸再真正重建预取。
    fn apply_pending_resize(&mut self, now: Instant) {
        let Some((at, size)) = self.pending_resize else {
            return;
        };
        let due = at + RESIZE_SETTLE;
        if now < due {
            if self.next_frame_at > due {
                self.next_frame_at = due;
            }
            return;
        }
        self.pending_resize = None;
        if self.size == size {
            self.start_worker();
            self.next_frame_at = now;
        }
    }

    fn direction(&self) -> (f32, f32) {
        DIRECTIONS[self.slide_index % DIRECTIONS.len()]
    }

    fn take_next(&mut self, now: Instant) -> bool {
        let slide = if let Some(slide) = self.pending_slide.take() {
            slide
        } else {
            let Some(rx) = &self.rx else {
                return false;
            };
            match rx.try_recv() {
                Ok(slide) => slide,
                Err(_) => return false,
            }
        };

        if (slide.comp.width, slide.comp.height) != self.size {
            log_warn!(
                "丢弃尺寸不匹配的预取画面: {}x{}（当前 {}x{}）",
                slide.comp.width, slide.comp.height, self.size.0, self.size.1
            );
            return false;
        }

        log_info!("显示: {}", slide.item.title);
        status::update(&self.status, |current| {
            current.current = slide.item.title.clone();
        });
        self.slide_index += 1;

        let Some(gpu) = &self.gpu else {
            return false;
        };
        let new_tex =
            match gpu.upload(&slide.comp.background, &slide.comp.poster, slide.comp.position) {
                Ok(tex) => tex,
                Err(err) => {
                    log_error!("上传纹理失败: {err}");
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
        let elapsed = (now - start).as_secs_f64();
        // 切张只看 elapsed；Ken Burns 进度只负责动效。关掉动效时 progress 恒 0，
        // 但停留时间到了仍然要换下一张。
        let progress = if self.cfg.ken_burns {
            (elapsed / display).min(1.0) as f32
        } else {
            0.0
        };
        self.progress = progress;
        self.uv = self.uv_at(progress);

        if let (Some(gpu), Some(current)) = (&self.gpu, &self.current) {
            gpu.draw(current, self.uv, 1.0);
        }

        if elapsed >= display {
            if self.take_next(now) {
                return;
            }
            self.next_frame_at = now + Duration::from_millis(120);
        } else if self.cfg.ken_burns {
            let fps = self.cfg.motion_fps.max(1);
            self.next_frame_at = now + Duration::from_millis(1000 / fps as u64);
        } else {
            // 没有动效就不需要每 120ms 重画同一帧，直接等到该换张的时刻。
            // 常驻模式还要每 500ms 醒一次检查输入（尤其宽限期结束时），
            // 不能只靠窗口事件 —— 焦点万一不在自己身上就收不起来。
            let end = start + Duration::from_secs_f64(display);
            let mut next = end;
            if self.watch.is_some() {
                next = next.min(now + Duration::from_millis(500));
                if now < self.grace_until {
                    next = next.min(self.grace_until);
                }
            }
            self.next_frame_at = next;
        }
    }

    fn present(&mut self) {
        let Some(result) = self.gpu.as_ref().map(|gpu| gpu.present()) else {
            return;
        };
        let now = Instant::now();
        match result {
            Ok(PresentStatus::Presented) | Ok(PresentStatus::Occluded) => {
                if self.gpu_recovery.record_success() {
                    log_info!("GPU 已恢复，继续渲染");
                }
            }
            Err(err) if err.device_lost() => {
                self.gpu = None;
                self.current = None;
                self.phase = Phase::Idle;
                self.note_gpu_failure(now, &err);
                self.next_frame_at = now;
            }
            Err(err) => {
                // 非设备级错误不销毁设备，避免一次瞬时错误升级成完整重建。
                if let Some(suppressed) = self.transient_log.record(now, GPU_LOG_INTERVAL) {
                    if suppressed == 0 {
                        log_warn!("Present 暂时失败，保留当前设备重试: {err}");
                    } else {
                        log_warn!(
                            "Present 持续失败，保留当前设备: {err}；已抑制 {suppressed} 条"
                        );
                    }
                }
                self.next_frame_at = now + Duration::from_secs(1);
            }
        }
    }

    fn sync_status(&self, now: Instant) {
        let paused = self.pause.is_paused(now);
        let reason = self.pause.reason(now).to_string();
        status::update(&self.status, |current| {
            if current.paused != paused || current.pause_reason != reason {
                current.paused = paused;
                current.pause_reason = reason;
            }
        });
    }

    /// 只在常驻隐藏时轮询免打扰状态，避免把正在播放的屏保自己判成“全屏应用”。
    fn check_dnd(&mut self, now: Instant) {
        let hidden_watch = self.watch.as_ref().is_some_and(|watch| !watch.showing);
        if !hidden_watch || now < self.next_dnd_check {
            return;
        }
        self.next_dnd_check = now + Duration::from_secs(1);
        let reason = crate::platform::busy_reason();
        let busy = reason.is_some();
        if busy == self.pause.dnd {
            return;
        }
        self.pause.dnd = busy;
        if let Some(reason) = reason {
            log_info!("检测到全屏应用/演示（{reason}），暂停自动触发");
        } else {
            log_info!("全屏应用/演示结束，恢复自动触发");
            if let Some(watch) = &mut self.watch {
                watch.idle.reset(now);
                watch.next_poll = now + Duration::from_millis(500);
            }
        }
        self.sync_status(now);
    }

    fn on_system_pause(&mut self, now: Instant) {
        if self.watch.as_ref().is_some_and(|watch| watch.showing) {
            self.hide_screensaver(now);
        }
        self.stop_worker();
        self.gpu = None;
        self.gpu_recovery.reset();
        self.transient_log.reset();
        self.current = None;
        self.phase = Phase::Idle;
        self.next_frame_at = now;
        self.sync_status(now);
    }

    fn on_system_resume(&mut self, now: Instant) {
        // 休眠/锁屏后设备可能已失效，下一次按需重建交换链。
        self.gpu = None;
        self.gpu_recovery.reset();
        self.transient_log.reset();
        self.current = None;
        self.phase = Phase::Idle;
        if self.window.is_some() && !self.pause.system_paused() && self.rx.is_none() {
            self.start_worker();
        }
        if let Some(watch) = &mut self.watch {
            watch.idle.reset(now);
            watch.next_poll = now;
        }
        self.grace_until = now + Duration::from_millis(1500);
        self.next_frame_at = now;
        self.next_dnd_check = now;
        self.sync_status(now);
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
        log_info!(
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

impl ApplicationHandler<UserEvent> for App {
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let now = Instant::now();
        match event {
            UserEvent::StartNow => self.start_now(event_loop),
            UserEvent::PauseFor(duration) => {
                self.pause.manual_until = Some(now + duration);
                log_info!("手动暂停自动触发 {:.0} 分钟", duration.as_secs_f64() / 60.0);
                self.sync_status(now);
            }
            UserEvent::ResumeAuto => {
                self.pause.manual_until = None;
                log_info!("手动恢复自动触发");
                self.sync_status(now);
            }
            UserEvent::Quit => event_loop.exit(),
            UserEvent::SessionLocked => {
                self.pause.locked = true;
                log_info!("锁屏，暂停自动触发");
                self.on_system_pause(now);
            }
            UserEvent::SessionUnlocked => {
                self.pause.locked = false;
                log_info!("解除锁屏，恢复检测");
                self.on_system_resume(now);
            }
            UserEvent::PowerSuspend => {
                self.pause.suspended = true;
                log_info!("系统休眠，暂停自动触发");
                self.on_system_pause(now);
            }
            UserEvent::PowerResume => {
                self.pause.suspended = false;
                log_info!("系统唤醒，重建渲染器");
                self.on_system_resume(now);
            }
            UserEvent::DisplayOff => {
                self.pause.display_off = true;
                log_info!("显示器关闭，暂停自动触发");
                self.on_system_pause(now);
            }
            UserEvent::DisplayOn => {
                self.pause.display_off = false;
                log_info!("显示器恢复，恢复检测");
                self.on_system_resume(now);
            }
        }
    }

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
            Ok(w) => {
                let window = Arc::new(w);
                if let Some(icon) = window_icon() {
                    window.set_window_icon(Some(icon.clone()));
                    window.set_taskbar_icon(Some(icon));
                }
                window
            }
            Err(err) => {
                log_error!("创建窗口失败: {err}");
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

        if !watching {
            // 首次创建失败不退出：按统一退避在事件循环里重试，避免驱动瞬时故障直接结束进程。
            self.ensure_gpu();
        }

        if self.rx.is_none() {
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
            WindowEvent::Resized(size) => {
                // 常驻模式收起/隐藏过程中的窗口尺寸没有意义，等下次触发再按主屏重建。
                if self.watch.as_ref().is_some_and(|w| !w.showing) {
                    return;
                }
                if size.width == 0 || size.height == 0 {
                    return;
                }
                self.update_size((size.width, size.height), Instant::now(), false);
            }
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

        // 窗口尺寸稳定后再重建交换链/预取线程。
        self.apply_pending_resize(now);
        self.pause.clear_expired(now);
        self.check_dnd(now);
        // 隐藏等待时也预读一张，空闲到点就能判断有没有内容可显示。
        self.poll_pending();
        let content = self.pending_slide.is_some() || self.current.is_some();
        let paused = self.pause.is_paused(now);

        // 常驻模式：藏着的时候数空闲，显示的时候盯输入
        let mut start = false;
        let mut stop = false;
        if let Some(w) = self.watch.as_mut() {
            if w.showing {
                if now < self.grace_until {
                    // 手动拉起时，宽限期里的输入（松开热键）要并进基准，
                    // 否则松开的那一下就会被当成「有人回来了」，下一秒就收起。
                    if w.manual {
                        w.dismiss_tick = idle::last_input_tick();
                    }
                } else {
                    // 检测失败时不知道有没有输入：不收起，也不拿失败值当基准。
                    match (idle::last_input_tick(), w.dismiss_tick) {
                        (Some(tick), Some(base)) if tick != base => stop = true,
                        (Some(tick), None) => w.dismiss_tick = Some(tick),
                        _ => {}
                    }
                }
            } else if now >= w.next_poll {
                w.next_poll = now + Duration::from_millis(500);
                match w.idle.poll(now) {
                    Some(idle) => {
                        w.idle_failed = false;
                        if std::env::var_os("PPS_DEBUG").is_some() {
                            log_info!(
                                "[watch] 已空闲 {:.1}s / 阈值 {:.0}s",
                                idle.as_secs_f64(),
                                w.threshold.as_secs_f64()
                            );
                        }
                        if idle >= w.threshold {
                            if paused {
                                // 暂停期间保持空闲计时；恢复后不需要再等一个完整阈值。
                                w.next_poll = now + Duration::from_secs(5);
                                w.waiting_content = false;
                            } else if content {
                                log_info!("空闲 {:.0} 秒，开始屏保", idle.as_secs_f64());
                                start = true;
                            } else {
                                // 已经空闲到点但 Plex/缓存还没内容：保留 idle 计时，
                                // 内容一到就能触发，而不是再等一个完整阈值。
                                if !w.waiting_content {
                                    w.waiting_content = true;
                                    log_warn!("已空闲但暂无可播放的海报，等待 Plex 恢复");
                                }
                                w.next_poll = now + Duration::from_secs(5);
                            }
                        } else {
                            w.waiting_content = false;
                        }
                    }
                    None => {
                        // 拿不到「最后输入时间」时暂停自动触发，避免把开机时长
                        // 误当成空闲时间；热键/再启动仍然可以手动拉起。
                        if !w.idle_failed {
                            w.idle_failed = true;
                            log_warn!(
                                "空闲检测不可用（GetLastInputInfo 失败），暂停自动触发；仍可用热键拉起"
                            );
                        }
                    }
                }
            }
        }
        if start {
            self.show_screensaver(event_loop, now, false);
        }
        if stop {
            self.hide_screensaver(now);
        }
        self.sync_status(now);
        if let Some(w) = &self.watch {
            if !w.showing {
                event_loop.set_control_flow(ControlFlow::WaitUntil(w.next_poll));
                return;
            }
        }

        // 锁屏/休眠/显示器关闭时，普通全屏模式也不要在后台继续 Present。
        if self.pause.system_paused() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_secs(1)));
            return;
        }

        if self.window.is_some() && self.active() && self.gpu.is_none() && self.ensure_gpu() {
            self.next_frame_at = now;
        }

        if self.window.is_some() && self.gpu.is_some() && now >= self.next_frame_at {
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

        // 设备退避期间不要按帧空转：把唤醒时间推到下次允许尝试的时刻。
        // 事件（退出、输入、系统通知）仍会正常唤醒事件循环。
        let wake = if self.gpu.is_none() {
            self.gpu_recovery
                .retry_at()
                .unwrap_or_else(|| now + Duration::from_secs(1))
        } else {
            self.next_frame_at
        };
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_backoff_follows_schedule_and_caps() {
        let expected = [1u64, 2, 4, 8, 16, 32, 60, 60];
        for (index, secs) in expected.iter().enumerate() {
            assert_eq!(
                gpu_backoff(index as u32 + 1),
                Duration::from_secs(*secs)
            );
        }
    }

    #[test]
    fn gpu_recovery_keeps_backoff_until_present_succeeds() {
        let now = Instant::now();
        let mut recovery = GpuRecovery::default();
        assert!(recovery.can_attempt(now));

        assert_eq!(recovery.record_failure(now), Some(0), "首次故障立即记录");
        assert!(!recovery.can_attempt(now));
        assert!(recovery.can_attempt(now + Duration::from_secs(1)));
        assert_eq!(recovery.retry_in(now), Duration::from_secs(1));

        // 1 秒内的再次失败只累计，不再写日志，退避继续增长。
        assert_eq!(
            recovery.record_failure(now + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            recovery.retry_in(now + Duration::from_secs(1)),
            Duration::from_secs(2)
        );

        // “设备创建成功”不会调用 record_success；下一次 Present 失败仍继续退避。
        assert_eq!(
            recovery.record_failure(now + Duration::from_secs(3)),
            None
        );
        assert_eq!(
            recovery.retry_in(now + Duration::from_secs(3)),
            Duration::from_secs(4)
        );

        assert!(recovery.record_success(), "成功呈现后应报告恢复");
        assert!(!recovery.record_success(), "正常状态不应重复报告");
        assert!(recovery.can_attempt(now + Duration::from_secs(3)));
    }

    #[test]
    fn gpu_recovery_logs_summary_after_interval() {
        let now = Instant::now();
        let mut recovery = GpuRecovery::default();
        assert_eq!(recovery.record_failure(now), Some(0));
        for step in 1..=3 {
            assert_eq!(
                recovery.record_failure(now + Duration::from_millis(step * 100)),
                None
            );
        }
        assert_eq!(
            recovery.record_failure(now + GPU_LOG_INTERVAL),
            Some(3),
            "30 秒窗口到期后应输出汇总"
        );
    }

    #[test]
    fn two_minute_failure_simulation_follows_backoff() {
        let start = Instant::now();
        let mut now = start;
        let mut recovery = GpuRecovery::default();
        let mut attempts = Vec::new();
        let mut logs = 0;

        // 以 100ms 为事件循环步长模拟连续失败 2 分钟。
        while now.duration_since(start) < Duration::from_secs(120) {
            if recovery.can_attempt(now) {
                attempts.push(now.duration_since(start).as_secs());
                if recovery.record_failure(now).is_some() {
                    logs += 1;
                }
            }
            now += Duration::from_millis(100);
        }

        assert_eq!(attempts, vec![0, 1, 3, 7, 15, 31, 63]);
        assert!(logs <= 5, "2 分钟内日志应受限，实际写了 {logs} 条");
    }
}

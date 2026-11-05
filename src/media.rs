//! 媒体获取与预取：按媒体库轮询、在线/离线回退、海报墙组帧。
//!
//! 这里不接触窗口、事件循环和 D3D 状态；`App` 只通过 `Source::next`
//! 和后台线程通道拿已经组合好的 `Slide`。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::RgbImage;

use crate::config::DisplayLayout;
use crate::layout::WALL_POSTERS;
use crate::logging::{log_info, log_warn};
use crate::plex::{Cache, Client, MediaItem, Section};
use crate::render::{self, Frame};
use crate::status::{self, Shared};

pub struct Slide {
    /// 单张布局 1 张；海报墙最多 6 张（少于 6 张时按可用数量组帧）。
    pub items: Vec<MediaItem>,
    pub frame: Frame,
}

impl Slide {
    pub fn size(&self) -> (u32, u32) {
        self.frame.size()
    }

    /// 托盘/日志里显示的标题；海报墙只列第一张并标明总数。
    pub fn title(&self) -> String {
        match self.items.as_slice() {
            [] => String::new(),
            [only] => only.title.clone(),
            [first, ..] => format!("{} 等 {} 张", first.title, self.items.len()),
        }
    }
}

const TASK_CANCELLED: &str = "预取任务已取消";

/// 海报墙取图方式：在线可下载补齐，离线只读缓存。
#[derive(Clone, Copy, PartialEq, Eq)]
enum WallLoadMode {
    Online,
    Cached,
}

/// 取消与提交共用的闸门。
///
/// 提交必须在锁内检查代次并执行；取消/换代也要拿同一把锁。这样“检查通过后、
/// 真正写缓存前”被取消的窗口就消失了：要么提交先完成（取消等它），要么取消先
/// 完成（提交看到代次已变，直接放弃）。
#[derive(Default)]
pub(crate) struct TaskGate {
    current: AtomicU64,
    commit: Mutex<()>,
}

impl TaskGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.commit
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// 使当前代次失效，并返回下一个可用代次。
    pub(crate) fn advance(&self) -> u64 {
        let _guard = self.lock();
        self.current.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn commit<T>(&self, generation: u64, f: impl FnOnce() -> T) -> Option<T> {
        let _guard = self.lock();
        if self.current.load(Ordering::Acquire) != generation {
            return None;
        }
        Some(f())
    }
}

/// 预取任务代次。只有当前代次且未被取消的任务才能提交缓存、状态和画面。
#[derive(Clone)]
pub(crate) struct TaskGuard {
    generation: u64,
    gate: Arc<TaskGate>,
    cancel: Arc<AtomicBool>,
}

impl TaskGuard {
    /// 工具命令/自检使用的常驻有效令牌。
    pub(crate) fn detached() -> Self {
        Self {
            generation: 0,
            gate: Arc::new(TaskGate::default()),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn new(generation: u64, gate: Arc<TaskGate>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            generation,
            gate,
            cancel,
        }
    }

    pub(crate) fn is_current(&self) -> bool {
        !self.cancel.load(Ordering::Relaxed)
            && self.gate.current.load(Ordering::Acquire) == self.generation
    }

    /// 在取消/换代锁内提交；取消发生后返回 None，闭包不会执行。
    fn commit<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        if self.cancel.load(Ordering::Relaxed) {
            return None;
        }
        self.gate.commit(self.generation, f)
    }
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
    /// 单张或海报墙；决定每次取一张图还是一组图。
    layout: DisplayLayout,
    status: Shared,
    guard: TaskGuard,
}

impl Source {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Client,
        cache: Cache,
        sections: Vec<Section>,
        size: (u32, u32),
        wanted: Vec<String>,
        status: Shared,
        layout: DisplayLayout,
    ) -> Self {
        Self::with_guard(
            client,
            cache,
            sections,
            size,
            wanted,
            status,
            layout,
            TaskGuard::detached(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_guard(
        client: Client,
        cache: Cache,
        sections: Vec<Section>,
        size: (u32, u32),
        wanted: Vec<String>,
        status: Shared,
        layout: DisplayLayout,
        guard: TaskGuard,
    ) -> Self {
        let buffers = sections
            .iter()
            .map(|s| (s.key.clone(), VecDeque::new()))
            .collect();
        let online = !sections.is_empty();
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
            layout,
            status,
            guard,
        };
        source.commit_status(|current| {
            current.online = online;
            if online {
                current.last_error.clear();
            }
        });
        source.reload_offline();
        source
    }

    fn ensure_current(&self) -> Result<(), String> {
        if self.guard.is_current() {
            Ok(())
        } else {
            Err(TASK_CANCELLED.to_string())
        }
    }

    fn commit_status(&self, f: impl FnOnce(&mut status::Status)) {
        self.guard.commit(|| status::update(&self.status, f));
    }

    /// 在取消/换代锁内提交缓存变更；过期任务不会执行闭包。
    fn commit_cache<T>(&mut self, f: impl FnOnce(&mut Cache) -> T) -> Option<T> {
        let guard = self.guard.clone();
        guard.commit(|| f(&mut self.cache))
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
        if !self.guard.is_current() {
            return;
        }
        if self.online {
            log_warn!("Plex 暂时取不到图，改用本地缓存离线播放");
        }
        self.online = false;
        self.next_reconnect = Instant::now() + RECONNECT_INTERVAL;
        self.commit_status(|current| {
            current.online = false;
            if current.last_error.is_empty() {
                current.last_error = "Plex 连接不可用，使用本地缓存".to_string();
            }
        });
        self.reload_offline();
    }

    fn try_reconnect(&mut self, now: Instant) {
        self.next_reconnect = now + RECONNECT_INTERVAL;
        let sections = match self.client.sections() {
            Ok(sections) => sections,
            Err(err) => {
                if self.guard.is_current() {
                    log_warn!("重连 Plex 失败: {err}");
                    self.commit_status(|current| {
                        current.last_error = format!("重连 Plex 失败: {err}");
                    });
                }
                return;
            }
        };
        // 网络请求期间可能已被取消：过期任务不得写索引或改状态。
        if !self.guard.is_current() {
            return;
        }
        if sections.is_empty() {
            return;
        }
        // 记下完整的库列表，离线时才能把 --library 的编号解析回库。
        // 与取消在同一闸门内提交，避免取消后旧任务仍写 sections。
        if self
            .commit_cache(|cache| cache.remember_sections(&sections))
            .is_none()
        {
            return;
        }
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
        self.commit_status(|current| {
            current.online = true;
            current.last_error.clear();
        });
    }

    fn remember(&mut self, key: &str) {
        self.recent.push_back(key.to_string());
        if self.recent.len() > 12 {
            self.recent.pop_front();
        }
    }

    fn finish(&mut self, items: Vec<MediaItem>, frame: Frame) -> Slide {
        for item in &items {
            self.remember(&item.rating_key);
        }
        Slide { items, frame }
    }

    /// 在线取一张。只有连续多次取图失败才切离线，单个坏条目不至于让整个屏保定格。
    fn online_next(&mut self) -> Result<Option<Slide>, String> {
        self.ensure_current()?;
        if self.sections.is_empty() {
            self.go_offline();
            return Ok(None);
        }

        let mut errors = 0;
        let mut empty_rounds = 0;
        for attempt in 0..300 {
            self.ensure_current()?;
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
                        if !self.guard.is_current() {
                            return Err(TASK_CANCELLED.to_string());
                        }
                        log_warn!("补充取图列表失败: {err}");
                        self.commit_status(|current| {
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

            match self.single_frame_for(&item) {
                Ok(frame) => {
                    self.ensure_current()?;
                    return Ok(Some(self.finish(vec![item], frame)));
                }
                Err(err) => {
                    if !self.guard.is_current() {
                        return Err(TASK_CANCELLED.to_string());
                    }
                    log_warn!("取图失败 {}: {err}", item.title);
                    self.commit_status(|current| {
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
    fn image_for(&mut self, item: &MediaItem) -> Result<RgbImage, String> {
        self.ensure_current()?;
        if let Some(path) = self.cache.get(item) {
            match render::decode_file(&path) {
                Ok(img) => {
                    self.ensure_current()?;
                    let _ = self.commit_cache(|cache| cache.remember(item));
                    return Ok(img);
                }
                Err(err) => {
                    if !self.guard.is_current() {
                        return Err(TASK_CANCELLED.to_string());
                    }
                    log_warn!("缓存损坏，删除后重新下载: {err}");
                    let _ = self.commit_cache(|cache| cache.discard(&path));
                }
            }
        }

        let data = self.client.fetch_cover(item, self.size.0, self.size.1)?;
        // 下载完先看任务是否还属于当前代次，过期任务不再解码/落盘。
        self.ensure_current()?;
        if data.is_empty() {
            return Err(format!("封面内容为空: {}", item.title));
        }
        // 先确认能解码再落盘，避免把坏内容存成 .jpg 后一直命中坏缓存。
        let img = render::decode(&data)?;
        // 解码后、提交缓存前再检查一次，避免旧任务把结果写进正式索引。
        self.ensure_current()?;
        if self
            .commit_cache(|cache| cache.store(item, &data))
            .is_none()
        {
            return Err(TASK_CANCELLED.to_string());
        }
        Ok(img)
    }

    /// 离线路径：只读缓存，绝不访问网络。
    fn cached_image(&mut self, item: &MediaItem) -> Result<RgbImage, String> {
        self.ensure_current()?;
        let path = self
            .cache
            .get(item)
            .ok_or_else(|| format!("缓存文件不存在: {}", item.title))?;
        match render::decode_file(&path) {
            Ok(img) => {
                self.ensure_current()?;
                let _ = self.commit_cache(|cache| cache.remember(item));
                Ok(img)
            }
            Err(err) => {
                if !self.guard.is_current() {
                    return Err(TASK_CANCELLED.to_string());
                }
                let _ = self.commit_cache(|cache| cache.discard(&path));
                Err(err)
            }
        }
    }

    fn single_frame_for(&mut self, item: &MediaItem) -> Result<Frame, String> {
        let img = self.image_for(item)?;
        Ok(Frame::Single(render::compose_single(
            img,
            self.size.0,
            self.size.1,
        )))
    }

    fn cached_single_frame(&mut self, item: &MediaItem) -> Result<Frame, String> {
        let img = self.cached_image(item)?;
        Ok(Frame::Single(render::compose_single(
            img,
            self.size.0,
            self.size.1,
        )))
    }

    fn offline_next(&mut self) -> Result<Slide, String> {
        self.ensure_current()?;
        if self.offline.is_empty() {
            self.reload_offline();
        }
        let count = self.offline.len();
        for _ in 0..count {
            self.ensure_current()?;
            let Some(item) = self.offline.pop_front() else {
                break;
            };
            match self.cached_single_frame(&item) {
                Ok(frame) => {
                    self.ensure_current()?;
                    return Ok(self.finish(vec![item], frame));
                }
                Err(err) => {
                    if !self.guard.is_current() {
                        return Err(TASK_CANCELLED.to_string());
                    }
                    log_warn!("离线缓存不可用 {}: {err}", item.title);
                }
            }
        }
        let message = "无法连接 Plex，本地缓存也没有可用封面".to_string();
        self.commit_status(|current| {
            current.last_error = message.clone();
        });
        Err(message)
    }

    /// 从在线缓冲区里凑一组不重复的条目；网络失败或库存不足时返回已凑到的部分。
    fn online_wall_items(&mut self) -> Vec<MediaItem> {
        if self.sections.is_empty() {
            return Vec::new();
        }
        let mut picked: Vec<MediaItem> = Vec::with_capacity(WALL_POSTERS);
        let mut empty_rounds = 0usize;
        let mut attempts = 0usize;
        while picked.len() < WALL_POSTERS && attempts < 300 {
            attempts += 1;
            if !self.guard.is_current() {
                break;
            }
            let section = self.sections[self.rr % self.sections.len()].clone();
            self.rr = self.rr.wrapping_add(1);
            let key = section.key.clone();

            let empty = self.buffers.get(&key).map(|b| b.is_empty()).unwrap_or(true);
            if empty {
                match self.refill(&section) {
                    Ok(0) => {
                        empty_rounds += 1;
                        if empty_rounds >= self.sections.len() {
                            log_warn!("Plex 媒体库里没有条目，改用本地缓存");
                            self.go_offline();
                            break;
                        }
                        continue;
                    }
                    Ok(_) => empty_rounds = 0,
                    Err(err) => {
                        if !self.guard.is_current() {
                            break;
                        }
                        log_warn!("补充取图列表失败: {err}");
                        self.commit_status(|current| {
                            current.last_error = format!("补充取图列表失败: {err}");
                        });
                        self.go_offline();
                        break;
                    }
                }
            }

            let Some(item) = self.buffers.get_mut(&key).and_then(|b| b.pop_front()) else {
                continue;
            };
            // 同一面墙里不出现重复海报。
            if picked
                .iter()
                .any(|picked| picked.rating_key == item.rating_key)
            {
                continue;
            }
            // 优先避开最近放过的条目；小库或试得够多时允许重复，保证墙面有内容。
            if self.recent.contains(&item.rating_key) && attempts < 120 {
                continue;
            }
            picked.push(item);
        }
        picked
    }

    /// 取离线缓存候选，最多 `want` 张，跳过已加载的条目。
    ///
    /// 在线取图中途失败或数量不足时用它在不重复的前提下补齐墙面。
    pub(crate) fn take_offline_items(&mut self, want: usize, exclude: &[String]) -> Vec<MediaItem> {
        if want == 0 {
            return Vec::new();
        }
        if self.offline.is_empty() {
            self.reload_offline();
        }
        let mut picked: Vec<MediaItem> = Vec::with_capacity(want);
        let available = self.offline.len();
        for _ in 0..available {
            if picked.len() >= want {
                break;
            }
            let Some(item) = self.offline.pop_front() else {
                break;
            };
            if exclude.iter().any(|key| key == &item.rating_key) {
                continue;
            }
            if picked
                .iter()
                .any(|picked| picked.rating_key == item.rating_key)
            {
                continue;
            }
            picked.push(item);
        }
        picked
    }

    /// 逐张取图；单张失败只记录并跳过，保留已经成功缓存/下载的海报。
    ///
    /// 返回失败张数，供调用方汇总日志；取消时立即返回，由调用方再检查代次。
    fn load_wall_images(
        &mut self,
        items: Vec<MediaItem>,
        mode: WallLoadMode,
        loaded: &mut Vec<(MediaItem, RgbImage)>,
    ) -> usize {
        let mut errors = 0usize;
        for item in items {
            if loaded.len() >= WALL_POSTERS || !self.guard.is_current() {
                return errors;
            }
            if loaded
                .iter()
                .any(|(picked, _)| picked.rating_key == item.rating_key)
            {
                continue;
            }
            let result = match mode {
                WallLoadMode::Online => self.image_for(&item),
                WallLoadMode::Cached => self.cached_image(&item),
            };
            match result {
                Ok(img) => loaded.push((item, img)),
                Err(err) => {
                    if !self.guard.is_current() {
                        return errors;
                    }
                    log_warn!("海报墙取图失败 {}: {err}", item.title);
                    errors += 1;
                }
            }
        }
        errors
    }

    /// 用已加载的一组海报组帧；1 张时退回单张布局，0 张由调用方报错。
    fn compose_wall_slide(&mut self, loaded: Vec<(MediaItem, RgbImage)>) -> Result<Slide, String> {
        let items: Vec<MediaItem> = loaded.iter().map(|(item, _)| item.clone()).collect();
        let images: Vec<RgbImage> = loaded.into_iter().map(|(_, img)| img).collect();
        self.ensure_current()?;
        let frame = render::compose_wall_frame(images, self.size.0, self.size.1)?;
        if let Frame::Wall(wall) = &frame {
            log_info!("海报墙组帧完成：{} 张海报", wall.cards);
        }
        self.ensure_current()?;
        Ok(self.finish(items, frame))
    }

    fn wall_next(&mut self) -> Result<Slide, String> {
        let mut loaded: Vec<(MediaItem, RgbImage)> = Vec::with_capacity(WALL_POSTERS);
        let mut errors = 0usize;

        if self.online {
            let candidates = self.online_wall_items();
            // 取列表过程中可能已经切到离线：已选中的条目仍然保留，
            // 但不再为未缓存的封面等待网络超时；能命中缓存的继续组帧。
            let mode = if self.online {
                WallLoadMode::Online
            } else {
                WallLoadMode::Cached
            };
            errors += self.load_wall_images(candidates, mode, &mut loaded);
            // 取消时不要再消费离线队列。
            self.ensure_current()?;
        }

        // 在线候选不足或部分取图失败时，用离线缓存补齐，
        // 避免“刚选中的条目尚未缓存”导致整组组帧失败。
        if loaded.len() < WALL_POSTERS {
            let exclude: Vec<String> = loaded
                .iter()
                .map(|(item, _)| item.rating_key.clone())
                .collect();
            let offline = self.take_offline_items(WALL_POSTERS - loaded.len(), &exclude);
            errors += self.load_wall_images(offline, WallLoadMode::Cached, &mut loaded);
        }

        self.ensure_current()?;
        if loaded.is_empty() {
            let message = if self.online {
                "Plex 暂时没有可用的海报，本地也没有可用缓存".to_string()
            } else {
                "无法连接 Plex，本地缓存也没有可用封面".to_string()
            };
            self.commit_status(|current| {
                current.last_error = message.clone();
            });
            return Err(message);
        }
        if errors > 0 {
            log_warn!(
                "海报墙有 {errors} 张取图失败，用剩余 {} 张继续组帧",
                loaded.len()
            );
        }

        let slide = self.compose_wall_slide(loaded)?;
        let online = self.online;
        self.commit_status(|current| {
            current.online = online;
            current.last_error.clear();
        });
        Ok(slide)
    }

    pub fn next(&mut self) -> Result<Slide, String> {
        self.ensure_current()?;
        let now = Instant::now();
        self.cache.maintain_if_due();
        if !self.online && now >= self.next_reconnect {
            self.try_reconnect(now);
        }
        if self.layout == DisplayLayout::Wall {
            return self.wall_next();
        }
        if self.online {
            match self.online_next() {
                Ok(Some(slide)) => {
                    self.commit_status(|current| {
                        current.online = true;
                        current.last_error.clear();
                    });
                    return Ok(slide);
                }
                Ok(None) => {}
                Err(err) => {
                    if !self.guard.is_current() {
                        return Err(TASK_CANCELLED.to_string());
                    }
                    log_warn!("在线取图失败: {err}");
                    self.commit_status(|current| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::thread;

    use crate::config::Config;

    #[test]
    fn task_guard_rejects_stale_generation() {
        let gate = Arc::new(TaskGate::default());
        let cancel = Arc::new(AtomicBool::new(false));

        let generation = gate.advance();
        let guard = TaskGuard::new(generation, gate.clone(), cancel.clone());
        assert!(guard.is_current());

        let next_generation = gate.advance();
        assert!(!guard.is_current(), "旧代次必须失效");

        let current_guard = TaskGuard::new(next_generation, gate.clone(), cancel.clone());
        assert!(current_guard.is_current());
        cancel.store(true, Ordering::Relaxed);
        assert!(!current_guard.is_current(), "取消标志也必须生效");

        assert!(TaskGuard::detached().is_current());
    }

    #[test]
    fn task_gate_serializes_cancel_with_inflight_commit() {
        let gate = Arc::new(TaskGate::default());
        let generation = gate.advance();
        let cancel = Arc::new(AtomicBool::new(false));
        let guard = TaskGuard::new(generation, gate.clone(), cancel);

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let committed = Arc::new(AtomicBool::new(false));

        let worker = {
            let guard = guard.clone();
            let committed = committed.clone();
            thread::spawn(move || {
                let result = guard.commit(|| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    committed.store(true, Ordering::SeqCst);
                });
                assert!(result.is_some(), "在取消前开始的提交应完成");
            })
        };

        started_rx.recv().unwrap();
        let gate_for_cancel = gate.clone();
        let canceller = thread::spawn(move || {
            gate_for_cancel.advance();
        });

        // 提交闭包仍持有闸门锁，取消必须等它完成。
        thread::sleep(Duration::from_millis(50));
        assert!(!committed.load(Ordering::SeqCst));

        release_tx.send(()).unwrap();
        worker.join().unwrap();
        canceller.join().unwrap();
        assert!(committed.load(Ordering::SeqCst));

        assert!(
            guard.commit(|| ()).is_none(),
            "取消完成后旧任务不能再提交缓存/状态"
        );
    }

    fn media(key: &str, title: &str) -> MediaItem {
        MediaItem {
            rating_key: key.to_string(),
            title: title.to_string(),
            thumb: String::new(),
            section_key: String::new(),
            section_title: String::new(),
        }
    }

    fn wall_frame(cards: usize) -> Frame {
        Frame::Wall(crate::render::WallComposition {
            canvas: vec![0; 4],
            width: 2,
            height: 2,
            cards,
        })
    }

    #[test]
    fn slide_title_summarizes_wall_content() {
        let single = Slide {
            items: vec![media("1", "阿凡达")],
            frame: wall_frame(1),
        };
        assert_eq!(single.title(), "阿凡达");

        let wall = Slide {
            items: vec![
                media("1", "阿凡达"),
                media("2", "盗梦空间"),
                media("3", "星际穿越"),
            ],
            frame: wall_frame(3),
        };
        assert_eq!(wall.title(), "阿凡达 等 3 张");

        let empty = Slide {
            items: Vec::new(),
            frame: wall_frame(0),
        };
        assert_eq!(empty.title(), "");
    }

    fn wall_scratch_dir(tag: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("wall-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 生成一张可解码的小 PNG，返回字节；临时源文件不留在缓存目录里。
    fn png_bytes(dir: &std::path::Path, tag: &str, rgb: [u8; 3]) -> Vec<u8> {
        let path = dir.join(format!("{tag}.png"));
        let img = RgbImage::from_pixel(40, 60, image::Rgb(rgb));
        image::save_buffer(&path, img.as_raw(), 40, 60, image::ColorType::Rgb8).unwrap();
        let data = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        data
    }

    fn wall_source(cache: Cache, size: (u32, u32)) -> Source {
        Source::new(
            Client::new(&Config::default()),
            cache,
            Vec::new(),
            size,
            Vec::new(),
            status::new(),
            DisplayLayout::Wall,
        )
    }

    fn seed_wall_cache(dir: &std::path::Path, colors: &[[u8; 3]]) -> Cache {
        let mut cache = Cache::open(dir, 30, crate::config::Naming::Title).unwrap();
        for (index, rgb) in colors.iter().enumerate() {
            let item = media(&format!("W{index}"), &format!("Wall {index}"));
            let data = png_bytes(dir, &format!("src{index}"), *rgb);
            cache.store(&item, &data).unwrap();
        }
        cache
    }

    #[test]
    fn wall_falls_back_to_offline_cache_when_plex_is_gone() {
        let dir = wall_scratch_dir("offline");
        let cache = seed_wall_cache(&dir, &[[200, 40, 40], [40, 200, 40], [40, 40, 200]]);
        let mut source = wall_source(cache, (320, 180));

        let slide = source.next().expect("离线缓存应能组帧");
        assert_eq!(slide.items.len(), 3, "应使用全部 3 张离线缓存");
        match &slide.frame {
            Frame::Wall(wall) => {
                assert_eq!(wall.cards, 3);
                assert_eq!((wall.width, wall.height), (320, 180));
            }
            Frame::Single(_) => panic!("3 张应走海报墙"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wall_skips_corrupt_cache_and_keeps_remaining_posters() {
        let dir = wall_scratch_dir("corrupt");
        let mut cache = seed_wall_cache(&dir, &[[200, 40, 40], [40, 200, 40]]);
        // 损坏文件仍然在索引里，但解码失败时应被跳过，而不是拖垮整组。
        cache
            .store(&media("W2", "Broken"), b"not a real image")
            .unwrap();
        let mut source = wall_source(cache, (320, 180));

        let slide = source.next().expect("坏缓存不应阻断整面墙");
        assert_eq!(slide.items.len(), 2);
        assert!(slide.items.iter().all(|item| item.rating_key != "W2"));
        match &slide.frame {
            Frame::Wall(wall) => assert_eq!(wall.cards, 2),
            Frame::Single(_) => panic!("2 张应走海报墙"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offline_backfill_skips_items_already_loaded() {
        let dir = wall_scratch_dir("exclude");
        let cache = seed_wall_cache(
            &dir,
            &[[200, 40, 40], [40, 200, 40], [40, 40, 200], [200, 200, 40]],
        );
        let mut source = wall_source(cache, (320, 180));

        let picked = source.take_offline_items(3, &["W0".to_string()]);
        assert_eq!(picked.len(), 3);
        assert!(
            picked.iter().all(|item| item.rating_key != "W0"),
            "已加载的条目不能重复补进海报墙"
        );
        let mut keys: Vec<_> = picked.iter().map(|item| item.rating_key.clone()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 3, "补位候选不能有重复");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! 封面磁盘缓存：文件命名、索引、容量/天数清理和离线索引。
//!
//! Plex HTTP 客户端和媒体模型在 `plex.rs`；本模块只碰磁盘和缓存内存索引。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Naming;
use crate::logging::{log_info, log_warn};
use crate::plex::{filter_sections, MediaItem, Section};

/// 本地索引文件名：记录已经缓存过哪些媒体条目，断网时用它枚举缓存。
const INDEX_FILE: &str = "index.json";
const INDEX_VERSION: u32 = 1;
/// 索引最多保留多少条，避免缓存目录长期累积后文件无限增长。
const MAX_INDEX_ITEMS: usize = 5000;
/// 临时文件序号，避免同进程内并发写入共用同一个 `.part`/`.tmp`。
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    #[serde(default)]
    items: Vec<MediaItem>,
    /// 上次在线时看到的完整媒体库列表，离线时用来解析 --library 的编号/标题。
    #[serde(default)]
    sections: Vec<Section>,
}

#[derive(Debug, Default)]
struct IndexData {
    items: Vec<MediaItem>,
    sections: Vec<Section>,
}

/// 封面缓存。默认按「标题_ratingKey」命名，同时认旧版的纯标题和纯 ratingKey 命名。
///
/// 除封面文件外，还在缓存目录里维护一份 `index.json`：记录每个封面对应的
/// ratingKey / 标题 / thumb。这样 Plex 断网时也能从磁盘枚举出可播放的条目。
///
/// 索引放在 `Arc<Mutex<_>>` 里：同一进程内的工作线程、新旧预取线程共享同一份
/// 索引，读取/合并/落盘都在锁内完成，避免旧线程用启动时的快照覆盖新条目。
#[derive(Debug, Clone)]
pub struct Cache {
    pub dir: PathBuf,
    naming: Naming,
    max_age: Option<Duration>,
    pub hits: u64,
    pub misses: u64,
    index: Arc<Mutex<IndexData>>,
    /// 容量上限（字节）；None 表示不限
    max_bytes: Option<u64>,
    last_maintain: Instant,
    stores_since_maintain: u32,
}

impl Cache {
    /// 严格模式：配置/命令行显式指定的目录，写不进去就报错，绝不偷偷换地方。
    pub fn open(dir: &Path, days: i64, naming: Naming) -> Result<Self, String> {
        if !writable(dir) {
            return Err(format!(
                "缓存目录不可写: {}
                 原因: 这个进程当前只能写项目目录（exe 放在项目目录里启动时会被系统限制）。
                 三种解决办法:
                 1. 把 exe 复制到项目外运行，缓存就能按配置写入
                 2. 把 config.json 的 cache_dir 改到可写目录
                 3. 加 --cache-dir auto，让程序自己挑一个可写目录
                 4. 如果 exe 是从网络/别的电脑拷来的，这个文件可能被系统标记为受限：
                    右键 -> 属性 -> 勾选「解除锁定」，或直接从编译输出重新复制一份",
                dir.display()
            ));
        }
        Ok(Self::at(dir, days, naming))
    }

    /// 自动模式：配置目录 -> exe 同目录 -> %LOCALAPPDATA% -> 纯内存
    pub fn auto(preferred: &Path, days: i64, naming: Naming) -> Self {
        let candidates = [
            preferred.to_path_buf(),
            crate::config::exe_dir().join("cache"),
            crate::config::app_dir().join("covers"),
        ];
        for candidate in candidates {
            if writable(&candidate) {
                if candidate != preferred {
                    log_warn!(
                        "{} 不可写，缓存改存到 {}",
                        preferred.display(),
                        candidate.display()
                    );
                }
                return Self::at(&candidate, days, naming);
            }
        }
        log_warn!("没有可写的缓存目录，改用内存缓存（显示正常，重启后会重新下载）");
        Self {
            dir: PathBuf::new(),
            naming,
            max_age: age_limit(days),
            hits: 0,
            misses: 0,
            index: Arc::new(Mutex::new(IndexData::default())),
            max_bytes: None,
            last_maintain: Instant::now(),
            stores_since_maintain: 0,
        }
    }

    fn at(dir: &Path, days: i64, naming: Naming) -> Self {
        let (mut index, sections) = load_index(dir);
        // 兼容升级前已有的缓存：没有 index.json（或索引不全）时从文件名恢复条目。
        merge_scanned_index(dir, &mut index);
        Self {
            dir: dir.to_path_buf(),
            naming,
            max_age: age_limit(days),
            hits: 0,
            misses: 0,
            index: Arc::new(Mutex::new(IndexData {
                items: index,
                sections,
            })),
            max_bytes: None,
            last_maintain: Instant::now(),
            stores_since_maintain: 0,
        }
    }

    fn lock_index(&self) -> MutexGuard<'_, IndexData> {
        self.index
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// 记录上次在线看到的完整媒体库列表；离线时用来解析 --library 的编号/标题。
    pub fn remember_sections(&mut self, sections: &[Section]) {
        if !self.is_disk() || sections.is_empty() {
            return;
        }
        let mut data = self.lock_index();
        if data.sections == sections {
            return;
        }
        data.sections = sections.to_vec();
        self.save_index(&data);
    }

    /// 本地索引里还有对应缓存文件的条目；断网时按这个列表播放。
    ///
    /// `wanted` 为空表示不过滤；否则用上次保存的媒体库列表解析编号/标题，
    /// 只返回属于选中媒体库的条目。
    pub fn offline_items(&self, wanted: &[String]) -> Vec<MediaItem> {
        // 先复制索引再查文件，避免长时间持有索引锁。
        let (items, sections) = {
            let data = self.lock_index();
            (data.items.clone(), data.sections.clone())
        };
        let selected = selected_sections(&sections, wanted);
        items
            .into_iter()
            .filter(|item| self.find(item).is_some() && item_matches(item, wanted, &selected))
            .collect()
    }

    /// 记住一个封面已经缓存过的媒体条目，写进 `index.json`。
    pub fn remember(&mut self, item: &MediaItem) {
        if !self.is_disk() {
            return;
        }
        let mut data = self.lock_index();
        if merge_item(&mut data.items, item) {
            self.save_index(&data);
        }
    }

    /// 删除一个损坏的缓存文件；索引条目会在下次在线取图时重新写回。
    pub fn discard(&self, path: &Path) {
        let _ = fs::remove_file(path);
    }

    /// 在索引锁内落盘；调用方已经持有 `IndexData` 的锁。
    fn save_index(&self, data: &IndexData) {
        if !self.is_disk() {
            return;
        }
        let index = IndexFile {
            version: INDEX_VERSION,
            items: data.items.clone(),
            sections: data.sections.clone(),
        };
        let Ok(bytes) = serde_json::to_vec(&index) else {
            return;
        };
        let path = self.dir.join(INDEX_FILE);
        let tmp = self.temp_path("index", "tmp");
        if fs::write(&tmp, bytes).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    /// 生成唯一的临时文件名，供索引和海报写入共用。
    fn temp_path(&self, stem: &str, extension: &str) -> PathBuf {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.dir.join(format!(
            ".{stem}.{}.{seq}.{stamp}.{extension}",
            std::process::id()
        ))
    }

    /// 是否真的落盘（内存模式返回 false）
    pub fn is_disk(&self) -> bool {
        !self.dir.as_os_str().is_empty()
    }

    /// 当前缓存目录里的封面数量与总字节数（只统计 jpg）。
    pub fn stats(&self) -> (usize, u64) {
        if !self.is_disk() {
            return (0, 0);
        }
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return (0, 0);
        };
        let mut count = 0;
        let mut bytes = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file()
                || !path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
            {
                continue;
            }
            count += 1;
            bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        }
        (count, bytes)
    }

    /// 清空当前磁盘缓存：删除封面和未完成临时文件，保留媒体库列表索引。
    pub fn clear(&mut self) -> Result<(usize, u64), String> {
        if !self.is_disk() {
            return Err("当前缓存是内存模式，没有可清理的文件".to_string());
        }
        let entries = fs::read_dir(&self.dir).map_err(|err| format!("读取缓存目录失败: {err}"))?;
        let mut removed = 0usize;
        let mut bytes = 0u64;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let extension = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase());
            if !matches!(extension.as_deref(), Some("jpg" | "part")) {
                continue;
            }
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            if fs::remove_file(&path).is_ok() {
                removed += 1;
                bytes += size;
            }
        }
        {
            let mut data = self.lock_index();
            data.items.clear();
            self.save_index(&data);
        }
        self.hits = 0;
        self.misses = 0;
        self.stores_since_maintain = 0;
        self.last_maintain = Instant::now();
        Ok((removed, bytes))
    }

    /// 新建缓存时用的文件名。
    pub fn path_for(&self, item: &MediaItem) -> PathBuf {
        self.dir.join(format!("{}.jpg", self.preferred_stem(item)))
    }

    fn preferred_stem(&self, item: &MediaItem) -> String {
        let key = key_stem(item);
        match self.naming {
            Naming::Key => key,
            Naming::Title => {
                let title = sanitize_title(&item.title, MAX_TITLE_CHARS);
                if title.is_empty() {
                    key
                } else {
                    titled(&title, &key)
                }
            }
        }
    }

    /// 查找顺序：当前格式 -> 另一种新格式 -> 旧版纯标题 -> 旧版纯 ratingKey。
    fn candidates(&self, item: &MediaItem) -> Vec<PathBuf> {
        let key = key_stem(item);
        let new_title = sanitize_title(&item.title, MAX_TITLE_CHARS);
        let old_title = legacy_title_stem(&item.title);

        let mut stems: Vec<String> = Vec::new();
        match self.naming {
            Naming::Title => {
                push_stem(&mut stems, titled(&new_title, &key));
                push_stem(&mut stems, titled(&old_title, &key));
                push_stem(&mut stems, key.clone());
                push_stem(&mut stems, old_title);
            }
            Naming::Key => {
                push_stem(&mut stems, key.clone());
                push_stem(&mut stems, titled(&new_title, &key));
                push_stem(&mut stems, titled(&old_title, &key));
                push_stem(&mut stems, old_title);
            }
        }
        stems
            .into_iter()
            .map(|stem| self.dir.join(format!("{stem}.jpg")))
            .collect()
    }

    /// 只查找、不重命名、不计命中次数；离线索引过滤用。
    fn find(&self, item: &MediaItem) -> Option<PathBuf> {
        if !self.is_disk() {
            return None;
        }
        let wanted = self.path_for(item);
        if is_usable(&wanted) {
            return Some(wanted);
        }
        self.candidates(item)
            .into_iter()
            .find(|candidate| *candidate != wanted && is_usable(candidate))
    }

    /// Plex 的 thumb 路径带版本号；版本变了说明海报被换过，旧文件应当视为未命中。
    fn thumb_changed(&self, item: &MediaItem) -> bool {
        let data = self.lock_index();
        data.items
            .iter()
            .find(|existing| existing.rating_key == item.rating_key)
            .is_some_and(|existing| {
                !existing.thumb.is_empty() && !item.thumb.is_empty() && existing.thumb != item.thumb
            })
    }

    pub fn get(&mut self, item: &MediaItem) -> Option<PathBuf> {
        if !self.is_disk() {
            return None;
        }
        if self.thumb_changed(item) {
            return None;
        }
        let wanted = self.path_for(item);
        if is_usable(&wanted) {
            self.hits += 1;
            return Some(wanted);
        }
        for candidate in self.candidates(item) {
            if candidate == wanted || !is_usable(&candidate) {
                continue;
            }
            // 命中旧命名顺手改成当前格式，下次就不用再找
            self.hits += 1;
            if fs::rename(&candidate, &wanted).is_ok() {
                return Some(wanted);
            }
            return Some(candidate);
        }
        None
    }

    /// 写缓存；目录不可写时返回 None，调用方退回内存解码，不影响播放。
    pub fn store(&mut self, item: &MediaItem, data: &[u8]) -> Option<PathBuf> {
        if !self.is_disk() {
            return None;
        }
        let path = self.path_for(item);
        // 唯一临时文件：新旧预取线程同时写同一张图也不会互相截断。
        let tmp = self.temp_path("poster", "part");
        if fs::write(&tmp, data).is_err() {
            let _ = fs::remove_file(&tmp);
            return None;
        }
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
            // 另一个写者可能已经提交了同一张图；目标可用就视为成功。
            if !is_usable(&path) {
                return None;
            }
        }
        self.misses += 1;
        self.stores_since_maintain = self.stores_since_maintain.saturating_add(1);
        self.remember(item);
        Some(path)
    }

    /// 设置缓存容量上限；0 表示不限。
    pub fn set_max_bytes(&mut self, bytes: u64) {
        self.max_bytes = if bytes == 0 { None } else { Some(bytes) };
    }

    /// 运行期间定时维护：新下载积累到一定数量或距离上次清理超过一小时才真正扫描。
    pub fn maintain_if_due(&mut self) {
        if !self.is_disk() {
            return;
        }
        if self.stores_since_maintain >= 50
            || self.last_maintain.elapsed() >= Duration::from_secs(3600)
        {
            self.maintain();
        }
    }

    /// 清理过期文件、按容量上限删除最旧的本程序缓存，并同步裁剪索引。
    ///
    /// 只处理本程序自己命名的文件，旧版 Python 留下的纯标题文件一律不动。
    pub fn maintain(&mut self) {
        if !self.is_disk() {
            return;
        }

        let mut removed = 0usize;

        if let Some(max_age) = self.max_age {
            if let Ok(entries) = fs::read_dir(&self.dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !is_cache_file(&path) {
                        continue;
                    }
                    if let Ok(meta) = path.metadata() {
                        if let Ok(modified) = meta.modified() {
                            let expired = SystemTime::now()
                                .duration_since(modified)
                                .map(|age| age > max_age)
                                .unwrap_or(false);
                            if expired && fs::remove_file(&path).is_ok() {
                                removed += 1;
                            }
                        }
                    }
                }
            }
        }

        if let Some(max_bytes) = self.max_bytes {
            let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
            let mut total = 0u64;
            if let Ok(entries) = fs::read_dir(&self.dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !is_cache_file(&path) {
                        continue;
                    }
                    let Ok(meta) = path.metadata() else { continue };
                    let size = meta.len();
                    total = total.saturating_add(size);
                    files.push((
                        path,
                        size,
                        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    ));
                }
            }
            if total > max_bytes {
                files.sort_by_key(|(_, _, modified)| *modified);
                for (path, size, _) in files {
                    if total <= max_bytes {
                        break;
                    }
                    if fs::remove_file(&path).is_ok() {
                        total = total.saturating_sub(size);
                        removed += 1;
                    }
                }
            }
        }

        // 索引过滤与落盘在锁内完成；锁只覆盖内存索引和文件提交，不覆盖网络。
        let mut data = self.lock_index();
        let old_len = data.items.len();
        data.items.retain(|item| self.find(item).is_some());
        if removed > 0 || data.items.len() != old_len {
            log_info!(
                "缓存维护完成：删除 {removed} 个文件，索引 {} 条",
                data.items.len()
            );
            self.save_index(&data);
        }
        drop(data);
        self.last_maintain = Instant::now();
        self.stores_since_maintain = 0;
    }
}

/// 解析 `wanted` 对应的媒体库 key 列表；没有保存过库列表时返回 None，
/// 由调用方退回按条目里的库标题匹配。
fn selected_sections(sections: &[Section], wanted: &[String]) -> Option<Vec<String>> {
    if wanted.is_empty() {
        return None;
    }
    let chosen = filter_sections(sections, wanted);
    if chosen.is_empty() && sections.is_empty() {
        // 没有保存过媒体库列表（旧缓存）：只能退回到按条目里保存的库标题匹配。
        return None;
    }
    Some(chosen.into_iter().map(|s| s.key).collect())
}

/// 把条目合并进索引；返回是否发生了变化。
///
/// 规则：空媒体库字段不覆盖已知值；标题/thumb 变化时更新；新条目追加并限制总量。
fn merge_item(items: &mut Vec<MediaItem>, item: &MediaItem) -> bool {
    if let Some(existing) = items.iter_mut().find(|i| i.rating_key == item.rating_key) {
        // 旧索引没有媒体库字段；在线播放命中缓存时也要把空字段补齐。
        // 反过来，扫描旧文件得到的条目字段是空的，不能拿它覆盖已有元数据。
        let section_changed = (!item.section_key.is_empty()
            && existing.section_key != item.section_key)
            || (!item.section_title.is_empty() && existing.section_title != item.section_title);
        if existing.title == item.title && existing.thumb == item.thumb && !section_changed {
            return false;
        }
        existing.title = item.title.clone();
        existing.thumb = item.thumb.clone();
        if !item.section_key.is_empty() {
            existing.section_key = item.section_key.clone();
        }
        if !item.section_title.is_empty() {
            existing.section_title = item.section_title.clone();
        }
        return true;
    }
    items.push(item.clone());
    if items.len() > MAX_INDEX_ITEMS {
        items.remove(0);
    }
    true
}

fn is_cache_file(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("jpg")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(is_ours_stem)
            .unwrap_or(false)
}

fn age_limit(days: i64) -> Option<Duration> {
    if days > 0 {
        Some(Duration::from_secs(days as u64 * 86400))
    } else {
        None
    }
}

/// 离线条目是否属于用户选中的媒体库。
fn item_matches(item: &MediaItem, wanted: &[String], selected: &Option<Vec<String>>) -> bool {
    if wanted.is_empty() {
        return true;
    }
    if let Some(keys) = selected {
        // 有媒体库列表就严格按 key 过滤；旧条目没有 section_key 时不猜。
        return keys.iter().any(|key| key == &item.section_key);
    }
    title_matches(item, wanted)
}

fn title_matches(item: &MediaItem, wanted: &[String]) -> bool {
    if item.section_title.is_empty() {
        return false;
    }
    wanted.iter().any(|token| {
        let token = token.trim();
        if token.is_empty() || token.parse::<usize>().is_ok() {
            return false;
        }
        item.section_title == token
            || item
                .section_title
                .to_lowercase()
                .contains(&token.to_lowercase())
    })
}

/// 没有索引（或索引缺条目）时，从缓存文件名恢复离线播放需要的 MediaItem。
///
/// 文件名只能还原出 ratingKey 和标题，`thumb` 留空 —— 离线播放不需要它，
/// 只有重新在线下载时才需要，而那时条目会从 Plex 重新拿到完整信息。
fn merge_scanned_index(dir: &Path, index: &mut Vec<MediaItem>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jpg") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_ours_stem(stem) {
            continue;
        }
        let (title, key) = split_stem(stem);
        if key.is_empty() || index.iter().any(|i| i.rating_key == key) {
            continue;
        }
        index.push(MediaItem {
            rating_key: key,
            title,
            thumb: String::new(),
            section_key: String::new(),
            section_title: String::new(),
        });
    }
}

/// `标题_编号` -> (标题, 编号)；纯编号文件 -> ("", 编号)。
fn split_stem(stem: &str) -> (String, String) {
    if let Some((title, key)) = stem.rsplit_once('_') {
        if !title.is_empty() && is_key_like(key) {
            return (title.to_string(), key.to_string());
        }
    }
    (String::new(), stem.to_string())
}

fn is_key_like(raw: &str) -> bool {
    is_all_digits(raw)
        || (raw.len() >= 8
            && raw.starts_with('h')
            && raw[1..].chars().all(|c| c.is_ascii_hexdigit()))
}

/// 读本地索引；不存在、损坏或版本不认识时都退化成空索引，不影响正常在线播放。
fn load_index(dir: &Path) -> (Vec<MediaItem>, Vec<Section>) {
    let path = dir.join(INDEX_FILE);
    let Ok(text) = fs::read_to_string(&path) else {
        return (Vec::new(), Vec::new());
    };
    match serde_json::from_str::<IndexFile>(&text) {
        Ok(file) if file.version == INDEX_VERSION => (file.items, file.sections),
        Ok(file) => {
            log_warn!(
                "缓存索引版本不支持，已忽略 {}（version={}）",
                path.display(),
                file.version
            );
            (Vec::new(), Vec::new())
        }
        Err(err) => {
            log_warn!("缓存索引损坏，已忽略 {}: {err}", path.display());
            (Vec::new(), Vec::new())
        }
    }
}

/// 真正试写一下，避免 create_dir_all 成功但文件写不进去。
fn writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write_probe");
    match fs::write(&probe, b"1") {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn is_usable(path: &Path) -> bool {
    path.metadata().map(|m| m.len() > 0).unwrap_or(false)
}

/// 标题部分最多保留多少字符，免得超长片名撑爆 Windows 的 260 字节路径。
const MAX_TITLE_CHARS: usize = 60;

/// `标题_编号`；标题为空就退化成纯编号。
fn titled(title: &str, key: &str) -> String {
    if title.is_empty() {
        String::new()
    } else {
        format!("{title}_{key}")
    }
}

/// ratingKey 规范化成文件名片段；实在拿不到就退回 thumb 的哈希。
fn key_stem(item: &MediaItem) -> String {
    let key: String = item
        .rating_key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if key.is_empty() {
        format!("h{:x}", fnv1a(item.thumb.as_bytes()))
    } else {
        key
    }
}

/// 标题清洗：去掉 Windows 不允许的字符（`\ / : * ? " < > |`）和控制字符，
/// 连续空格压成一个，砍掉首尾的空格和点（结尾带点/空格的文件名 Windows 不接受），并限长。
fn sanitize_title(title: &str, max_chars: usize) -> String {
    const FORBIDDEN: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

    let mut out = String::new();
    let mut kept = 0usize;
    let mut last_space = false;
    for ch in title.chars() {
        if kept >= max_chars {
            break;
        }
        if ch.is_control() || FORBIDDEN.contains(&ch) {
            continue;
        }
        // 全角空格也当空格处理，免得和 ASCII 空格混在一起
        let ch = if ch == '\u{3000}' { ' ' } else { ch };
        if ch == ' ' {
            if last_space || out.is_empty() {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
        kept += 1;
    }
    out.trim_end_matches([' ', '.']).to_string()
}

/// 旧版 v3/v4 用的标题规则（只留字母数字和空格、点、下划线），用来找老文件。
fn legacy_title_stem(title: &str) -> String {
    let safe: String = title
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '_'))
        .collect();
    safe.trim_end().to_string()
}

fn push_stem(stems: &mut Vec<String>, stem: String) {
    if !stem.is_empty() && !stems.contains(&stem) {
        stems.push(stem);
    }
}

fn is_all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// 是不是本程序生成的文件名，`prune` 只删这些。
fn is_ours_stem(stem: &str) -> bool {
    // 1.0：1479
    if is_all_digits(stem) {
        return true;
    }
    // 新版：标题_1479 / 标题_h1a2b3c4...
    if let Some((_, key)) = stem.rsplit_once('_') {
        if is_key_like(key) {
            return true;
        }
    }
    // ratingKey 为空时的兜底：h 加 fnv1a 十六进制
    match stem.strip_prefix('h') {
        Some(hex) => hex.len() >= 8 && hex.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::config::Naming;

    fn item(key: &str, title: &str) -> MediaItem {
        MediaItem {
            rating_key: key.to_string(),
            title: title.to_string(),
            thumb: format!("/library/metadata/{key}/thumb/1712345678"),
            section_key: "1".to_string(),
            section_title: "Movies".to_string(),
        }
    }

    #[test]
    fn title_is_cleaned_for_windows() {
        assert_eq!(sanitize_title("阿凡达: 水之道", 60), "阿凡达 水之道");
        assert_eq!(sanitize_title("A/B\\C*D?E\"F<G>H|I", 60), "ABCDEFGHI");
        assert_eq!(sanitize_title("  结局.  ", 60), "结局");
        assert_eq!(sanitize_title("甲    乙", 60), "甲 乙");
        assert_eq!(sanitize_title("", 60), "");
        assert_eq!(sanitize_title("...", 60), "");
    }

    #[test]
    fn long_title_is_capped() {
        let name = sanitize_title(&"阿".repeat(200), MAX_TITLE_CHARS);
        assert_eq!(name.chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn stems_follow_naming() {
        let dir = Path::new("C:/tmp");
        let by_title = Cache::at(dir, 30, Naming::Title);
        let by_key = Cache::at(dir, 30, Naming::Key);
        let it = item("1479", "阿凡达: 水之道");

        assert_eq!(by_title.preferred_stem(&it), "阿凡达 水之道_1479");
        assert_eq!(by_key.preferred_stem(&it), "1479");
        assert_eq!(
            by_title.path_for(&it),
            PathBuf::from("C:/tmp").join("阿凡达 水之道_1479.jpg")
        );
    }

    #[test]
    fn empty_title_falls_back_to_key() {
        let cache = Cache::at(Path::new("C:/tmp"), 30, Naming::Title);
        assert_eq!(cache.preferred_stem(&item("1479", "")), "1479");
        assert_eq!(cache.preferred_stem(&item("1479", "...")), "1479");
    }

    #[test]
    fn candidates_cover_both_legacy_names() {
        let cache = Cache::at(Path::new("C:/tmp"), 30, Naming::Title);
        let names: Vec<String> = cache
            .candidates(&item("1479", "阿凡达: 水之道"))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(names[0], "阿凡达 水之道_1479.jpg");
        assert!(names.contains(&"1479.jpg".to_string()));
        assert!(names.contains(&"阿凡达 水之道.jpg".to_string()));

        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), names.len(), "候选项不该重复: {names:?}");
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("cache-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn old_key_named_file_is_renamed() {
        let dir = scratch_dir("key");
        fs::write(dir.join("1479.jpg"), b"x").unwrap();

        let mut cache = Cache::at(&dir, 30, Naming::Title);
        let got = cache.get(&item("1479", "阿凡达")).unwrap();

        assert_eq!(got.file_name().unwrap(), "阿凡达_1479.jpg");
        assert!(dir.join("阿凡达_1479.jpg").is_file());
        assert!(!dir.join("1479.jpg").exists());
        assert_eq!(cache.hits, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_title_named_file_is_renamed() {
        let dir = scratch_dir("title");
        fs::write(dir.join("阿凡达 水之道.jpg"), b"x").unwrap();

        let mut cache = Cache::at(&dir, 30, Naming::Title);
        let got = cache.get(&item("1479", "阿凡达: 水之道")).unwrap();

        assert_eq!(got.file_name().unwrap(), "阿凡达 水之道_1479.jpg");
        assert!(!dir.join("阿凡达 水之道.jpg").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_a_miss() {
        let dir = scratch_dir("miss");
        let mut cache = Cache::at(&dir, 30, Naming::Title);
        assert!(cache.get(&item("1479", "阿凡达")).is_none());
        assert_eq!(cache.hits, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_survives_restart_and_filters_missing_files() {
        let dir = scratch_dir("index");
        let it = item("1479", "阿凡达");
        fs::write(dir.join("1479.jpg"), b"x").unwrap();

        let mut cache = Cache::at(&dir, 30, Naming::Title);
        cache.remember(&it);
        assert!(dir.join("index.json").is_file());

        // 模拟重启：重新从磁盘加载索引。
        let cache = Cache::at(&dir, 30, Naming::Title);
        let offline = cache.offline_items(&[]);
        assert_eq!(offline.len(), 1);
        assert_eq!(offline[0].rating_key, "1479");

        // 文件被清理后，索引条目不应再出现在可离线播放列表里。
        fs::remove_file(dir.join("1479.jpg")).unwrap();
        assert!(cache.offline_items(&[]).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_stats_and_clear_disk_files() {
        let dir = scratch_dir("clear");
        fs::write(dir.join("a.jpg"), b"abcd").unwrap();
        fs::write(dir.join("b.part"), b"12").unwrap();
        fs::write(dir.join("keep.txt"), b"x").unwrap();
        let mut cache = Cache::at(&dir, 30, Naming::Title);
        assert_eq!(cache.stats(), (1, 4));
        assert_eq!(cache.clear().unwrap(), (2, 6));
        assert!(!dir.join("a.jpg").exists());
        assert!(!dir.join("b.part").exists());
        assert!(dir.join("keep.txt").exists());
        assert_eq!(cache.stats(), (0, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn offline_items_respect_library_filter() {
        let dir = scratch_dir("offline-filter");
        let mut cache = Cache::at(&dir, 30, Naming::Title);

        let mut movie = item("101", "Movie");
        movie.section_key = "1".to_string();
        movie.section_title = "电影".to_string();
        let mut anime = item("201", "Anime");
        anime.section_key = "2".to_string();
        anime.section_title = "动漫".to_string();

        fs::write(cache.path_for(&movie), b"x").unwrap();
        fs::write(cache.path_for(&anime), b"x").unwrap();
        cache.remember(&movie);
        cache.remember(&anime);
        cache.remember_sections(&[
            Section {
                key: "1".to_string(),
                title: "电影".to_string(),
                kind: "movie".to_string(),
            },
            Section {
                key: "2".to_string(),
                title: "动漫".to_string(),
                kind: "show".to_string(),
            },
        ]);

        let by_title = cache.offline_items(&["动漫".to_string()]);
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].rating_key, "201");

        // 编号筛选要能在重启后靠索引里保存的媒体库列表解析。
        let reloaded = Cache::at(&dir, 30, Naming::Title);
        let by_index = reloaded.offline_items(&["2".to_string()]);
        assert_eq!(by_index.len(), 1);
        assert_eq!(by_index[0].rating_key, "201");
        assert_eq!(reloaded.offline_items(&[]).len(), 2);
        assert!(reloaded
            .offline_items(&["不存在的库".to_string()])
            .is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_index_gets_section_fields_when_cache_is_played() {
        let dir = scratch_dir("legacy-upgrade");
        let full = item("101", "Movie");
        // 模拟升级前的索引：没有 section_key / section_title。
        fs::write(
            dir.join("index.json"),
            format!(
                r#"{{"version":1,"items":[{{"rating_key":"101","title":"Movie","thumb":"{}"}}]}}"#,
                full.thumb
            ),
        )
        .unwrap();

        let mut cache = Cache::at(&dir, 30, Naming::Title);
        fs::write(cache.path_for(&full), b"x").unwrap();
        cache.remember_sections(&[
            Section {
                key: "1".to_string(),
                title: "Movies".to_string(),
                kind: "movie".to_string(),
            },
            Section {
                key: "2".to_string(),
                title: "Anime".to_string(),
                kind: "show".to_string(),
            },
        ]);
        assert!(cache.offline_items(&["Movies".to_string()]).is_empty());

        // 模拟在线播放命中这条旧缓存：remember 应补齐媒体库字段。
        cache.remember(&full);
        let reloaded = Cache::at(&dir, 30, Naming::Title);
        let offline = reloaded.offline_items(&["Movies".to_string()]);
        assert_eq!(offline.len(), 1);
        assert_eq!(offline[0].section_key, "1");

        // 扫描旧文件产生的空字段不能反过来把已补齐的字段覆盖掉。
        let mut reloaded = reloaded;
        let mut without_section = full.clone();
        without_section.section_key.clear();
        without_section.section_title.clear();
        reloaded.remember(&without_section);
        let again = Cache::at(&dir, 30, Naming::Title);
        assert_eq!(again.offline_items(&["Movies".to_string()]).len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_cache_files_are_scanned_for_offline_playback() {
        let dir = scratch_dir("scan");
        fs::write(dir.join("1479.jpg"), b"x").unwrap();
        fs::write(dir.join("阿凡达 水之道_2819.jpg"), b"x").unwrap();
        fs::write(dir.join("foreign.png"), b"x").unwrap();

        let cache = Cache::at(&dir, 30, Naming::Title);
        let items = cache.offline_items(&[]);
        assert!(items.iter().any(|i| i.rating_key == "1479"));
        assert!(items
            .iter()
            .any(|i| i.rating_key == "2819" && i.title == "阿凡达 水之道"));
        assert!(!items.iter().any(|i| i.rating_key == "foreign"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_only_touches_our_files() {
        assert!(is_ours_stem("1479"));
        assert!(is_ours_stem("阿凡达 水之道_1479"));
        assert!(is_ours_stem("h1a2b3c4d5e6f7a8b"));
        assert!(!is_ours_stem("蓝色巴士法国足球队罢练事件簿"));
        assert!(!is_ours_stem("Inception"));
    }

    #[test]
    fn changed_thumb_invalidates_cached_poster() {
        let dir = scratch_dir("thumb-change");
        let mut cache = Cache::at(&dir, 30, Naming::Key);
        let old = item("1479", "阿凡达");
        cache.store(&old, b"old-poster").unwrap();
        assert!(cache.get(&old).is_some());

        let mut refreshed = old.clone();
        refreshed.thumb = "/library/metadata/1479/thumb/9999999999".to_string();
        assert!(
            cache.get(&refreshed).is_none(),
            "Plex thumb 版本变化后旧缓存应视为未命中"
        );

        cache.store(&refreshed, b"new-poster").unwrap();
        assert!(cache.get(&refreshed).is_some());
        assert_eq!(fs::read(cache.path_for(&refreshed)).unwrap(), b"new-poster");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn maintain_enforces_cache_size_limit() {
        let dir = scratch_dir("capacity");
        let mut cache = Cache::at(&dir, 30, Naming::Key);
        for index in 0..3 {
            let it = item(&index.to_string(), &format!("Movie {index}"));
            cache.store(&it, &[b'x'; 100]).unwrap();
        }

        cache.set_max_bytes(150);
        cache.maintain();

        let total: u64 = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jpg"))
            .map(|entry| entry.metadata().unwrap().len())
            .sum();
        assert!(total <= 150, "清理后缓存仍超过上限: {total}");
        assert_eq!(cache.offline_items(&[]).len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_clones_share_index_updates() {
        let dir = scratch_dir("shared-index");
        let mut first = Cache::at(&dir, 30, Naming::Title);
        let mut second = first.clone();

        let mut movie = item("101", "Movie");
        movie.section_key = "1".to_string();
        movie.section_title = "电影".to_string();
        let mut anime = item("201", "Anime");
        anime.section_key = "2".to_string();
        anime.section_title = "动漫".to_string();

        first.store(&movie, b"movie-poster").unwrap();
        // second 是在 first 写入前克隆的；共享索引应立即看到新条目。
        assert_eq!(second.offline_items(&[]).len(), 1);
        second.store(&anime, b"anime-poster").unwrap();

        first.remember_sections(&[
            Section {
                key: "1".to_string(),
                title: "电影".to_string(),
                kind: "movie".to_string(),
            },
            Section {
                key: "2".to_string(),
                title: "动漫".to_string(),
                kind: "show".to_string(),
            },
        ]);

        // 重新打开模拟重启：两个条目和媒体库信息都要保留。
        let reopened = Cache::at(&dir, 30, Naming::Title);
        assert_eq!(reopened.offline_items(&[]).len(), 2);
        let movies = reopened.offline_items(&["电影".to_string()]);
        assert_eq!(movies.len(), 1);
        assert_eq!(movies[0].rating_key, "101");
        assert_eq!(movies[0].section_title, "电影");
        let anime_items = reopened.offline_items(&["2".to_string()]);
        assert_eq!(anime_items.len(), 1);
        assert_eq!(anime_items[0].rating_key, "201");
        assert_eq!(anime_items[0].section_key, "2");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_stores_from_clones_keep_both_entries() {
        let dir = scratch_dir("concurrent-store");
        let cache = Cache::at(&dir, 30, Naming::Title);
        let mut first = cache.clone();
        let mut second = cache.clone();

        let mut movie = item("101", "Movie");
        movie.section_key = "1".to_string();
        movie.section_title = "电影".to_string();
        let mut anime = item("201", "Anime");
        anime.section_key = "2".to_string();
        anime.section_title = "动漫".to_string();

        let a = std::thread::spawn(move || {
            for _ in 0..20 {
                first.store(&movie, b"movie-poster").unwrap();
            }
        });
        let b = std::thread::spawn(move || {
            for _ in 0..20 {
                second.store(&anime, b"anime-poster").unwrap();
            }
        });
        a.join().unwrap();
        b.join().unwrap();

        let reopened = Cache::at(&dir, 30, Naming::Title);
        assert_eq!(reopened.offline_items(&[]).len(), 2);
        assert!(reopened
            .offline_items(&["电影".to_string()])
            .iter()
            .any(|i| i.rating_key == "101"));
        assert!(reopened
            .offline_items(&["动漫".to_string()])
            .iter()
            .any(|i| i.rating_key == "201"));

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext == "part" || ext == "tmp")
            })
            .collect();
        assert!(leftovers.is_empty(), "不应留下临时文件: {leftovers:?}");

        let _ = fs::remove_dir_all(&dir);
    }
}

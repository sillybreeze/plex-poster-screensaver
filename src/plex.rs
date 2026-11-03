//! Plex API 客户端与封面缓存。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::{Config, Naming};

/// 本地索引文件名：记录已经缓存过哪些媒体条目，断网时用它枚举缓存。
const INDEX_FILE: &str = "index.json";
const INDEX_VERSION: u32 = 1;
/// 索引最多保留多少条，避免缓存目录长期累积后文件无限增长。
const MAX_INDEX_ITEMS: usize = 5000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub key: String,
    pub title: String,
    pub kind: String,
}

/// 按配置/命令行里的编号或标题筛选媒体库；`wanted` 为空表示全部。
pub fn filter_sections(sections: &[Section], wanted: &[String]) -> Vec<Section> {
    if wanted.is_empty() {
        return sections.to_vec();
    }

    let mut chosen: Vec<Section> = Vec::new();
    for token in wanted {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let found = if let Ok(index) = token.parse::<usize>() {
            sections.get(index.saturating_sub(1)).cloned()
        } else {
            sections
                .iter()
                .find(|s| s.title == token)
                .or_else(|| {
                    sections
                        .iter()
                        .find(|s| s.title.to_lowercase().contains(&token.to_lowercase()))
                })
                .cloned()
        };
        if let Some(section) = found {
            if !chosen.iter().any(|s| s.key == section.key) {
                chosen.push(section);
            }
        }
    }
    chosen
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub rating_key: String,
    pub title: String,
    pub thumb: String,
    /// 条目所属媒体库，离线播放时按它过滤；旧索引没有这两个字段，反序列化为空。
    #[serde(default)]
    pub section_key: String,
    #[serde(default)]
    pub section_title: String,
}

#[derive(Debug, Clone)]
pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: String,
    transcode: bool,
}

impl Client {
    pub fn new(cfg: &Config) -> Self {
        let mut builder = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(cfg.timeout))
            .timeout_read(Duration::from_secs(cfg.timeout.max(20)));

        if !cfg.verify_tls {
            if let Ok(tls) = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .build()
            {
                builder = builder.tls_connector(std::sync::Arc::new(tls));
            }
        }

        Self {
            agent: builder.build(),
            base: cfg.url.clone(),
            token: cfg.token.clone(),
            transcode: cfg.transcode,
        }
    }

    fn text(&self, path: &str, params: &[(&str, String)]) -> Result<String, String> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.get(&url).set("X-Plex-Token", &self.token);
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => resp
                .into_string()
                .map_err(|e| format!("读取响应失败: {e}")),
            Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
            Err(e) => Err(format!("请求失败: {e}")),
        }
    }

    fn bytes(&self, path: &str, params: &[(&str, String)]) -> Result<Vec<u8>, String> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.get(&url).set("X-Plex-Token", &self.token);
        for (k, v) in params {
            req = req.query(k, v);
        }
        let resp = match req.call() {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code}")),
            Err(e) => return Err(format!("请求失败: {e}")),
        };
        let mut buf = Vec::new();
        resp.into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("下载失败: {e}"))?;
        Ok(buf)
    }

    pub fn sections(&self) -> Result<Vec<Section>, String> {
        let xml = self.text("/library/sections", &[])?;
        let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("XML 解析失败: {e}"))?;

        let mut out = Vec::new();
        for node in doc.root_element().children().filter(|n| n.is_element()) {
            if node.tag_name().name() != "Directory" {
                continue;
            }
            let kind = node.attribute("type").unwrap_or("");
            if kind != "movie" && kind != "show" {
                continue;
            }
            out.push(Section {
                key: node.attribute("key").unwrap_or("").to_string(),
                title: node.attribute("title").unwrap_or("未命名").to_string(),
                kind: kind.to_string(),
            });
        }
        Ok(out)
    }

    /// 分页 + 服务端随机，只取一小批。Video（电影）和 Directory（剧集）都要。
    pub fn random_items(&self, section: &Section, count: usize) -> Result<Vec<MediaItem>, String> {
        let xml = self.text(
            &format!("/library/sections/{}/all", section.key),
            &[
                ("sort", "random".to_string()),
                ("X-Plex-Container-Start", "0".to_string()),
                ("X-Plex-Container-Size", count.to_string()),
            ],
        )?;
        let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("XML 解析失败: {e}"))?;

        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for node in doc.root_element().children().filter(|n| n.is_element()) {
            let tag = node.tag_name().name();
            if tag != "Video" && tag != "Directory" {
                continue;
            }
            let (Some(key), Some(title), Some(thumb)) = (
                node.attribute("ratingKey"),
                node.attribute("title"),
                node.attribute("thumb"),
            ) else {
                continue;
            };
            if !seen.insert(key.to_string()) {
                continue;
            }
            items.push(MediaItem {
                rating_key: key.to_string(),
                title: title.to_string(),
                thumb: thumb.to_string(),
                section_key: section.key.clone(),
                section_title: section.title.clone(),
            });
        }
        items.truncate(count);
        Ok(items)
    }

    /// 优先让服务器缩图；失败再取原图。
    pub fn fetch_cover(&self, item: &MediaItem, w: u32, h: u32) -> Result<Vec<u8>, String> {
        if self.transcode {
            let params = vec![
                ("url", item.thumb.clone()),
                ("width", w.to_string()),
                ("height", h.to_string()),
                ("minSize", "1".to_string()),
                ("upscale", "0".to_string()),
            ];
            if let Ok(data) = self.bytes("/photo/:/transcode", &params) {
                if !data.is_empty() {
                    return Ok(data);
                }
            }
        }
        self.bytes(&item.thumb, &[])
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    #[serde(default)]
    items: Vec<MediaItem>,
    /// 上次在线时看到的完整媒体库列表，离线时用来解析 --library 的编号/标题。
    #[serde(default)]
    sections: Vec<Section>,
}

/// 封面缓存。默认按「标题_ratingKey」命名，同时认旧版的纯标题和纯 ratingKey 命名。
///
/// 除封面文件外，还在缓存目录里维护一份 `index.json`：记录每个封面对应的
/// ratingKey / 标题 / thumb。这样 Plex 断网时也能从磁盘枚举出可播放的条目。
#[derive(Debug, Clone)]
pub struct Cache {
    pub dir: PathBuf,
    naming: Naming,
    max_age: Option<Duration>,
    pub hits: u64,
    pub misses: u64,
    index: Vec<MediaItem>,
    sections: Vec<Section>,
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
                    eprintln!(
                        "提示: {} 不可写，缓存改存到 {}",
                        preferred.display(),
                        candidate.display()
                    );
                }
                return Self::at(&candidate, days, naming);
            }
        }
        eprintln!("提示: 没有可写的缓存目录，改用内存缓存（显示正常，重启后会重新下载）");
        Self {
            dir: PathBuf::new(),
            naming,
            max_age: age_limit(days),
            hits: 0,
            misses: 0,
            index: Vec::new(),
            sections: Vec::new(),
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
            index,
            sections,
        }
    }

    /// 工作线程重启前从磁盘重新读一次索引；工作线程里的副本可能已经写过新条目。
    pub fn reload_index(&mut self) {
        if self.is_disk() {
            let (mut index, sections) = load_index(&self.dir);
            merge_scanned_index(&self.dir, &mut index);
            self.index = index;
            self.sections = sections;
        }
    }

    /// 记录上次在线看到的完整媒体库列表；离线时用来解析 --library 的编号/标题。
    pub fn remember_sections(&mut self, sections: &[Section]) {
        if !self.is_disk() || sections.is_empty() || self.sections == sections {
            return;
        }
        self.sections = sections.to_vec();
        self.save_index();
    }

    /// 本地索引里还有对应缓存文件的条目；断网时按这个列表播放。
    ///
    /// `wanted` 为空表示不过滤；否则用上次保存的媒体库列表解析编号/标题，
    /// 只返回属于选中媒体库的条目。
    pub fn offline_items(&self, wanted: &[String]) -> Vec<MediaItem> {
        let selected = self.selected_sections(wanted);
        self.index
            .iter()
            .filter(|item| self.find(item).is_some() && item_matches(item, wanted, &selected))
            .cloned()
            .collect()
    }

    fn selected_sections(&self, wanted: &[String]) -> Option<Vec<String>> {
        if wanted.is_empty() {
            return None;
        }
        let chosen = filter_sections(&self.sections, wanted);
        if chosen.is_empty() && self.sections.is_empty() {
            // 没有保存过媒体库列表（旧缓存）：只能退回到按条目里保存的库标题匹配。
            return None;
        }
        Some(chosen.into_iter().map(|s| s.key).collect())
    }

    /// 记住一个封面已经缓存过的媒体条目，写进 `index.json`。
    pub fn remember(&mut self, item: &MediaItem) {
        if !self.is_disk() {
            return;
        }
        if let Some(existing) = self
            .index
            .iter_mut()
            .find(|i| i.rating_key == item.rating_key)
        {
            // 旧索引没有媒体库字段；在线播放命中缓存时也要把空字段补齐。
            // 反过来，扫描旧文件得到的条目字段是空的，不能拿它覆盖已有元数据。
            let section_changed = (!item.section_key.is_empty()
                && existing.section_key != item.section_key)
                || (!item.section_title.is_empty()
                    && existing.section_title != item.section_title);
            if existing.title != item.title
                || existing.thumb != item.thumb
                || section_changed
            {
                existing.title = item.title.clone();
                existing.thumb = item.thumb.clone();
                // 空媒体库字段不覆盖已有值，避免扫描旧文件时把元数据冲掉。
                if !item.section_key.is_empty() {
                    existing.section_key = item.section_key.clone();
                }
                if !item.section_title.is_empty() {
                    existing.section_title = item.section_title.clone();
                }
                self.save_index();
            }
            return;
        }
        self.index.push(item.clone());
        if self.index.len() > MAX_INDEX_ITEMS {
            self.index.remove(0);
        }
        self.save_index();
    }

    /// 删除一个损坏的缓存文件；索引条目会在下次在线取图时重新写回。
    pub fn discard(&self, path: &Path) {
        let _ = fs::remove_file(path);
    }

    fn save_index(&self) {
        if !self.is_disk() {
            return;
        }
        let data = IndexFile {
            version: INDEX_VERSION,
            items: self.index.clone(),
            sections: self.sections.clone(),
        };
        let Ok(bytes) = serde_json::to_vec(&data) else {
            return;
        };
        let path = self.dir.join(INDEX_FILE);
        // 旧/新工作线程可能短暂并存，临时文件名带时间戳，避免互相覆盖同一个 .tmp。
        let stamp = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = self
            .dir
            .join(format!("index.{}.{stamp}.tmp", std::process::id()));
        if fs::write(&tmp, bytes).is_err() {
            return;
        }
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    /// 是否真的落盘（内存模式返回 false）
    pub fn is_disk(&self) -> bool {
        !self.dir.as_os_str().is_empty()
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

    pub fn get(&mut self, item: &MediaItem) -> Option<PathBuf> {
        if !self.is_disk() {
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
        let tmp = path.with_extension("part");
        if fs::write(&tmp, data).is_err() {
            return None;
        }
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
            return None;
        }
        self.misses += 1;
        self.remember(item);
        Some(path)
    }

    /// 只清理本程序自己命名的文件（纯 ratingKey、标题_ratingKey、哈希兜底），
    /// 旧版 Python 留下的纯标题文件一律不动。
    pub fn prune(&self) {
        if !self.is_disk() {
            return;
        }
        let Some(max_age) = self.max_age else { return };
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jpg") {
                continue;
            }
            let ours = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(is_ours_stem)
                .unwrap_or(false);
            if !ours {
                continue;
            }
            if let Ok(meta) = path.metadata() {
                if let Ok(modified) = meta.modified() {
                    if SystemTime::now()
                        .duration_since(modified)
                        .map(|age| age > max_age)
                        .unwrap_or(false)
                        && fs::remove_file(&path).is_ok()
                    {
                        removed += 1;
                    }
                }
            }
        }
        if removed > 0 {
            eprintln!("已清理 {removed} 个过期封面缓存");
        }
    }
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
            eprintln!(
                "缓存索引版本不支持，已忽略 {}（version={}）",
                path.display(),
                file.version
            );
            (Vec::new(), Vec::new())
        }
        Err(err) => {
            eprintln!("缓存索引损坏，已忽略 {}: {err}", path.display());
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
    fn sections_filter_by_index_or_title() {
        let sections = vec![
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
        ];
        assert_eq!(filter_sections(&sections, &[]).len(), 2);
        assert_eq!(filter_sections(&sections, &["2".to_string()])[0].key, "2");
        assert_eq!(filter_sections(&sections, &["ani".to_string()])[0].key, "2");
        assert!(filter_sections(&sections, &["missing".to_string()]).is_empty());
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
}

//! Plex API 客户端与封面缓存。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::{Config, Naming};

#[derive(Debug, Clone)]
pub struct Section {
    pub key: String,
    pub title: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct MediaItem {
    pub rating_key: String,
    pub title: String,
    pub thumb: String,
}

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
    pub fn random_items(&self, section: &str, count: usize) -> Result<Vec<MediaItem>, String> {
        let xml = self.text(
            &format!("/library/sections/{section}/all"),
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

/// 封面缓存。默认按「标题_ratingKey」命名，同时认旧版的纯标题和纯 ratingKey 命名。
pub struct Cache {
    pub dir: PathBuf,
    naming: Naming,
    max_age: Option<Duration>,
    pub hits: u64,
    pub misses: u64,
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
        }
    }

    fn at(dir: &Path, days: i64, naming: Naming) -> Self {
        Self {
            dir: dir.to_path_buf(),
            naming,
            max_age: age_limit(days),
            hits: 0,
            misses: 0,
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
    // 新版：标题_1479
    if let Some((_, key)) = stem.rsplit_once('_') {
        if is_all_digits(key) {
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
    fn prune_only_touches_our_files() {
        assert!(is_ours_stem("1479"));
        assert!(is_ours_stem("阿凡达 水之道_1479"));
        assert!(is_ours_stem("h1a2b3c4d5e6f7a8b"));
        assert!(!is_ours_stem("蓝色巴士法国足球队罢练事件簿"));
        assert!(!is_ours_stem("Inception"));
    }
}

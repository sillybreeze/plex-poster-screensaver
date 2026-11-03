//! Plex API 客户端与封面缓存。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;

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

/// 按 ratingKey 命名的封面缓存，兼容旧版按标题命名的文件。
pub struct Cache {
    pub dir: PathBuf,
    max_age: Option<Duration>,
    pub hits: u64,
    pub misses: u64,
}

impl Cache {
    /// 严格模式：配置/命令行显式指定的目录，写不进去就报错，绝不偷偷换地方。
    pub fn open(dir: &Path, days: i64) -> Result<Self, String> {
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
        Ok(Self::at(dir, days))
    }

    /// 自动模式：配置目录 -> exe 同目录 -> %LOCALAPPDATA% -> 纯内存
    pub fn auto(preferred: &Path, days: i64) -> Self {
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
                return Self::at(&candidate, days);
            }
        }
        eprintln!("提示: 没有可写的缓存目录，改用内存缓存（显示正常，重启后会重新下载）");
        Self {
            dir: PathBuf::new(),
            max_age: age_limit(days),
            hits: 0,
            misses: 0,
        }
    }

    fn at(dir: &Path, days: i64) -> Self {
        Self {
            dir: dir.to_path_buf(),
            max_age: age_limit(days),
            hits: 0,
            misses: 0,
        }
    }

    /// 是否真的落盘（内存模式返回 false）
    pub fn is_disk(&self) -> bool {
        !self.dir.as_os_str().is_empty()
    }

    pub fn path_for(&self, item: &MediaItem) -> PathBuf {
        let key: String = item
            .rating_key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        let name = if key.is_empty() {
            format!("h{:x}", fnv1a(item.thumb.as_bytes()))
        } else {
            key
        };
        self.dir.join(format!("{name}.jpg"))
    }

    /// 旧版 v3/v4 用「标题.jpg」命名，命中就改名成 ratingKey，避免重复下载。
    fn legacy_path(&self, item: &MediaItem) -> Option<PathBuf> {
        let safe: String = item
            .title
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '_'))
            .collect();
        let safe = safe.trim_end();
        if safe.is_empty() {
            return None;
        }
        Some(self.dir.join(format!("{safe}.jpg")))
    }

    pub fn get(&mut self, item: &MediaItem) -> Option<PathBuf> {
        if !self.is_disk() {
            return None;
        }
        let path = self.path_for(item);
        if is_usable(&path) {
            self.hits += 1;
            return Some(path);
        }
        if let Some(legacy) = self.legacy_path(item) {
            if is_usable(&legacy) {
                if fs::rename(&legacy, &path).is_ok() {
                    self.hits += 1;
                    return Some(path);
                }
                self.hits += 1;
                return Some(legacy);
            }
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

    /// 只清理本程序按 ratingKey 命名的文件，旧版标题命名的留给用户。
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
            let is_rating_key = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false);
            if !is_rating_key {
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

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

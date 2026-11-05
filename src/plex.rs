//! Plex HTTP 客户端与媒体模型；磁盘封面缓存在 `cache.rs`。

use std::io::Read;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;

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
            Ok(resp) => resp.into_string().map_err(|e| format!("读取响应失败: {e}")),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

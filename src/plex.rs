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

pub use crate::cache::Cache;

#[cfg(test)]
pub(crate) use crate::cache::{is_ours_stem, sanitize_title, MAX_TITLE_CHARS};

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

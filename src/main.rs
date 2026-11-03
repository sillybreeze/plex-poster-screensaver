//! plex-poster-screensaver（Rust 重写版）
//!
//! 从 Plex 媒体库随机取海报全屏轮播，自动提取主色做氛围光，并带 Ken Burns 缓慢推近。

mod app;
mod config;
mod gpu;

/// Windows 默认定时器精度约 15.6ms，会把 30fps 凑整成 15fps，动效一顿一顿的。
#[cfg(windows)]
mod timer {
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
        fn timeEndPeriod(period: u32) -> u32;
    }
    pub fn begin() {
        unsafe {
            timeBeginPeriod(1);
        }
    }
    pub fn end() {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

#[cfg(not(windows))]
mod timer {
    pub fn begin() {}
    pub fn end() {}
}
mod plex;
mod render;

use std::path::Path;
use std::time::Instant;

use config::{Args, Config};
use plex::{Cache, Client, Section};

fn main() {
    timer::begin();
    let args = match config::parse_args() {
        Ok(a) => a,
        Err(err) => {
            eprintln!("参数错误: {err}\n\n{}", config::HELP);
            std::process::exit(2);
        }
    };
    if args.help {
        print!("{}", config::HELP);
        return;
    }

    let cfg = config::load(&args);
    if cfg.token.is_empty() {
        eprintln!("缺少 Plex Token。请设置环境变量 PLEX_TOKEN，或在 config.json 中填写 plex_token。");
        std::process::exit(1);
    }

    let client = Client::new(&cfg);
    let sections = match client.sections() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("无法连接 Plex 服务器 {}: {err}", cfg.url);
            std::process::exit(1);
        }
    };
    if sections.is_empty() {
        eprintln!("服务器上没有找到「电影」或「剧集」类型的媒体库");
        std::process::exit(1);
    }

    let selected = match select_sections(&sections, &args, &cfg) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };

    if args.list {
        println!("Plex: {}", cfg.url);
        for (i, s) in selected.iter().enumerate() {
            let kind = if s.kind == "movie" { "电影" } else { "剧集" };
            println!("  {}. {}  [{}]  key={}", i + 1, s.title, kind, s.key);
        }
        return;
    }

    let cache = if cfg.cache_dir_explicit && !cfg.cache_dir_auto {
        match Cache::open(&cfg.cache_dir, cfg.cache_days) {
            Ok(cache) => cache,
            Err(err) => {
                eprintln!("{err}");
                std::process::exit(2);
            }
        }
    } else {
        Cache::auto(&cfg.cache_dir, cfg.cache_days)
    };
    cache.prune();

    if let Some(count) = args.self_test {
        run_self_test(&cfg, client, cache, selected, count);
        return;
    }
    if let Some(path) = &args.snapshot {
        run_snapshot(&cfg, client, cache, selected, path);
        return;
    }

    println!(
        "媒体库: {} | 缓存: {}",
        selected
            .iter()
            .map(|s| s.title.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        if cache.is_disk() {
            cache.dir.display().to_string()
        } else {
            "内存（不落盘）".to_string()
        }
    );

    let app = app::App::new(cfg, args.preview, client, cache, selected);
    let result = app.run();
    timer::end();
    if let Err(err) = result {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn select_sections(
    sections: &[Section],
    args: &Args,
    cfg: &Config,
) -> Result<Vec<Section>, String> {
    let wanted = if !args.library.is_empty() {
        args.library.clone()
    } else {
        cfg.libraries.clone()
    };
    if wanted.is_empty() {
        return Ok(sections.to_vec());
    }

    let mut chosen: Vec<Section> = Vec::new();
    for token in wanted {
        let token = token.trim().to_string();
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
        match found {
            Some(section) => {
                if !chosen.iter().any(|s| s.key == section.key) {
                    chosen.push(section);
                }
            }
            None => eprintln!("找不到媒体库: {token}"),
        }
    }

    if chosen.is_empty() {
        return Err(format!(
            "没有匹配到任何媒体库，可用的是: {}",
            sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(chosen)
}

fn run_self_test(cfg: &Config, client: Client, cache: Cache, sections: Vec<Section>, count: usize) {
    let size = (1920u32, 1080u32);
    let mut source = app::Source::new(client, cache, sections, size);

    println!("自检开始：{count} 张，屏幕 {}x{}", size.0, size.1);
    let mut total = 0.0;
    for i in 0..count {
        let started = Instant::now();
        match source.next() {
            Ok(slide) => {
                let elapsed = started.elapsed().as_secs_f64();
                total += elapsed;
                println!(
                    "[{:>2}/{}] {:6.0} ms | {}",
                    i + 1,
                    count,
                    elapsed * 1000.0,
                    slide.item.title
                );
            }
            Err(err) => {
                eprintln!("自检失败: {err}");
                std::process::exit(1);
            }
        }
    }
    println!(
        "平均 {:.0} ms/张；淡入淡出 {:.1} 秒，停留 {:.1} 秒",
        total / count as f64 * 1000.0,
        cfg.fade_seconds,
        cfg.display_seconds
    );
}

fn run_snapshot(
    _cfg: &Config,
    client: Client,
    cache: Cache,
    sections: Vec<Section>,
    path: &str,
) {
    let size = (1920u32, 1080u32);
    let mut source = app::Source::new(client, cache, sections, size);
    let slide = match source.next() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("取图失败: {err}");
            std::process::exit(1);
        }
    };

    let mut buffer = vec![0u32; (size.0 * size.1) as usize];
    render::draw_poster(&slide.comp, 0.0, (1.0, 0.0), &mut buffer, true);
    match render::save_png(&buffer, size.0, size.1, Path::new(path)) {
        Ok(()) => println!("已导出渲染结果: {path} （{}）", slide.item.title),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

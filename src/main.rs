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
mod idle;
mod logging;
mod platform;
mod plex;
mod render;
mod status;
mod trigger;

use std::path::Path;
use std::time::Instant;

use config::Config;
use logging::{log_error, log_warn};
use plex::{Cache, Client, Section};

fn main() {
    logging::init();

    let args = match config::parse_args() {
        Ok(a) => a,
        Err(err) => {
            log_error!("参数错误: {err}\n\n{}", config::HELP);
            std::process::exit(2);
        }
    };
    if args.help {
        print!("{}", config::HELP);
        return;
    }

    let config_path = config::locate_config(args.config.as_deref());

    // --help / --list / --self-test / --snapshot 是工具命令，不能被单实例逻辑拦住。
    // 只有真正要开窗口的模式才检查是否已有实例在跑。
    let utility = args.list || args.self_test.is_some() || args.snapshot.is_some();
    let trigger_event = if utility {
        None
    } else {
        if trigger::signal_existing() {
            println!("已经有一个实例在跑，已通知它立刻开始");
            return;
        }
        timer::begin();
        trigger::create()
    };

    let cfg = match config::load(&args) {
        Ok(cfg) => cfg,
        Err(err) => {
            log_error!("配置错误: {err}");
            std::process::exit(2);
        }
    };
    if cfg.token.is_empty() {
        log_error!("缺少 Plex Token。请设置环境变量 PLEX_TOKEN，或在 config.json 中填写 plex_token。");
        std::process::exit(1);
    }

    let wanted = if !args.library.is_empty() {
        args.library.clone()
    } else {
        cfg.libraries.clone()
    };

    let client = Client::new(&cfg);
    let sections = connect(&client, &cfg.url);
    let status_handle = status::new();
    status::update(&status_handle, |current| {
        current.online = !sections.is_empty();
    });

    let selected = if sections.is_empty() {
        // 连不上也不急着退出：本地索引里有缓存就先用缓存播放。
        Vec::new()
    } else {
        match select_sections(&sections, &wanted) {
            Ok(v) => v,
            Err(err) => {
                log_error!("{err}");
                std::process::exit(1);
            }
        }
    };

    if args.list {
        if selected.is_empty() {
            log_error!("无法连接 Plex 或没有可用媒体库");
            std::process::exit(1);
        }
        println!("Plex: {}", cfg.url);
        for (i, s) in selected.iter().enumerate() {
            let kind = if s.kind == "movie" { "电影" } else { "剧集" };
            println!("  {}. {}  [{}]  key={}", i + 1, s.title, kind, s.key);
        }
        return;
    }

    let mut cache = if cfg.cache_dir_explicit && !cfg.cache_dir_auto {
        match Cache::open(&cfg.cache_dir, cfg.cache_days, cfg.cache_naming) {
            Ok(cache) => cache,
            Err(err) => {
                log_error!("{err}");
                std::process::exit(2);
            }
        }
    } else {
        Cache::auto(&cfg.cache_dir, cfg.cache_days, cfg.cache_naming)
    };
    cache.set_max_bytes(cfg.cache_max_mb.saturating_mul(1024 * 1024));
    cache.maintain();
    // 保存完整媒体库列表，离线时才能把 --library 的编号/标题解析回库。
    cache.remember_sections(&sections);

    if selected.is_empty() {
        let offline = cache.offline_items(&wanted);
        if offline.is_empty() {
            let waiting = cfg.watch
                && !args.preview
                && args.self_test.is_none()
                && args.snapshot.is_none();
            if waiting {
                log_warn!(
                    "Plex 暂不可用，本地也没有可离线播放的缓存；常驻等待，后台会自动重连"
                );
            } else {
                log_error!(
                    "无法连接 Plex，本地也没有可离线播放的缓存: {}",
                    cache.dir.display()
                );
                std::process::exit(1);
            }
        } else {
            log_warn!(
                "Plex 暂不可用，先用本地缓存播放 {} 张；后台会自动重连",
                offline.len()
            );
        }
    }

    if let Some(count) = args.self_test {
        run_self_test(
            &cfg,
            client,
            cache,
            selected,
            wanted,
            status_handle,
            count,
        );
        return;
    }
    if let Some(path) = &args.snapshot {
        run_snapshot(
            &cfg,
            client,
            cache,
            selected,
            wanted,
            status_handle,
            path,
        );
        return;
    }

    let libraries = if selected.is_empty() {
        "离线缓存".to_string()
    } else {
        selected
            .iter()
            .map(|s| s.title.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "媒体库: {} | 缓存: {}",
        libraries,
        if cache.is_disk() {
            format!(
                "{}（{} 命名）",
                cache.dir.display(),
                cfg.cache_naming.as_str()
            )
        } else {
            "内存（不落盘）".to_string()
        }
    );

    let app = app::App::new(
        cfg,
        args.preview,
        client,
        cache,
        selected,
        wanted,
        status_handle,
        config_path,
    );
    let result = app.run(trigger_event);
    timer::end();
    if let Err(err) = result {
        log_error!("{err}");
        std::process::exit(1);
    }
}

/// 启动时尝试连接一次 Plex，拿媒体库列表。
///
/// 连不上就返回空列表，让主流程先走本地缓存；`Source` 会在后台每隔一段时间
/// 重新调用 `sections()`，恢复后自动切回在线取图。这样开机自启时网络/Plex
/// 还没起来也不会一直卡在启动阶段。
fn connect(client: &Client, url: &str) -> Vec<Section> {
    match client.sections() {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            log_warn!("服务器上没有找到「电影」或「剧集」类型的媒体库");
            Vec::new()
        }
        Err(err) => {
            log_warn!("无法连接 Plex 服务器 {url}: {err}");
            Vec::new()
        }
    }
}

fn select_sections(sections: &[Section], wanted: &[String]) -> Result<Vec<Section>, String> {
    if wanted.is_empty() {
        return Ok(sections.to_vec());
    }
    let chosen = plex::filter_sections(sections, wanted);
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

fn run_self_test(
    cfg: &Config,
    client: Client,
    cache: Cache,
    sections: Vec<Section>,
    wanted: Vec<String>,
    status: status::Shared,
    count: usize,
) {
    let size = (1920u32, 1080u32);
    let mut source = app::Source::new(client, cache, sections, size, wanted, status);

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
                log_error!("自检失败: {err}");
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
    wanted: Vec<String>,
    status: status::Shared,
    path: &str,
) {
    let size = (1920u32, 1080u32);
    let mut source = app::Source::new(client, cache, sections, size, wanted, status);
    let slide = match source.next() {
        Ok(s) => s,
        Err(err) => {
            log_error!("取图失败: {err}");
            std::process::exit(1);
        }
    };

    let mut buffer = vec![0u32; (size.0 * size.1) as usize];
    render::draw_poster(&slide.comp, 0.0, (1.0, 0.0), &mut buffer, true);
    match render::save_png(&buffer, size.0, size.1, Path::new(path)) {
        Ok(()) => println!("已导出渲染结果: {path} （{}）", slide.item.title),
        Err(err) => {
            log_error!("{err}");
            std::process::exit(1);
        }
    }
}

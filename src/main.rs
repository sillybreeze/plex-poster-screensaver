#![cfg_attr(
    all(windows, not(debug_assertions), not(test)),
    windows_subsystem = "windows"
)]

//! plex-poster-screensaver（Rust 重写版）
//!
//! 从 Plex 媒体库随机取海报全屏轮播，自动提取主色做氛围光，并带 Ken Burns 缓慢推近。

mod app;
mod autostart;
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
#[cfg(windows)]
mod console {
    use windows::core::w;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, FreeConsole, GetConsoleProcessList, GetStdHandle, SetStdHandle,
        ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ConsoleState {
        /// 进程没有控制台（例如计划任务/资源管理器启动）。
        None,
        /// 与 shell 共用控制台；不能动，否则会把用户终端一起关掉。
        Shared,
        /// 双击等场景单独分配的控制台，已经释放。
        Hidden,
    }

    fn valid_handle(handle: HANDLE) -> bool {
        !handle.0.is_null() && handle != INVALID_HANDLE_VALUE
    }

    /// release 版是 windows 子系统，不会给双击启动创建黑框。这里主动附着到
    /// 启动它的 cmd/PowerShell 控制台，恢复 `--help`/`--list` 等命令行输出。
    pub fn attach_parent() {
        unsafe {
            let Ok(()) = AttachConsole(ATTACH_PARENT_PROCESS) else {
                return;
            };
            // 如果父进程已经传了有效的重定向句柄（例如 `> out.txt`），保留它；
            // 否则用 CONOUT$ 补一个控制台句柄。
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE)
                .ok()
                .filter(|handle| valid_handle(*handle))
                .or_else(|| {
                    CreateFileW(
                        w!("CONOUT$"),
                        GENERIC_READ.0 | GENERIC_WRITE.0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        None,
                    )
                    .ok()
                });
            if let Some(handle) = stdout {
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
                let stderr = GetStdHandle(STD_ERROR_HANDLE)
                    .ok()
                    .filter(|handle| valid_handle(*handle));
                if stderr.is_none() {
                    let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
                }
            }
        }
    }

    /// 只释放进程独占的控制台。判断依据是同一控制台里是否还有别的进程：
    /// 从 cmd/PowerShell 启动时 shell 也在列表里，此时保留控制台和命令行输出。
    pub fn hide_if_private() -> ConsoleState {
        unsafe {
            let mut pids = [0u32; 16];
            let count = GetConsoleProcessList(&mut pids);
            if count == 0 {
                return ConsoleState::None;
            }
            if count == 1 {
                return match FreeConsole() {
                    Ok(()) => ConsoleState::Hidden,
                    Err(_) => ConsoleState::Shared,
                };
            }
            ConsoleState::Shared
        }
    }
}

#[cfg(not(windows))]
mod console {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ConsoleState {
        None,
        Shared,
        Hidden,
    }

    pub fn attach_parent() {}

    pub fn hide_if_private() -> ConsoleState {
        ConsoleState::None
    }
}

mod idle;
mod layout;
mod logging;
mod platform;
mod plex;
mod render;
mod settings;
mod settings_apply;
mod settings_preview;
mod status;
mod trigger;

use std::ffi::OsString;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use app::RunOutcome;
use config::{CacheLocation, Config};
use logging::{log_error, log_info, log_warn};
use plex::{Cache, Client, Section};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 记录致命错误；控制台不可见时补一个 MessageBox。
fn fatal_error(message: &str, code: i32, popup: bool) -> ! {
    log_error!("{message}");
    if popup {
        show_error(message);
    }
    std::process::exit(code);
}

fn show_error(message: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    let caption: Vec<u16> = "Plex 海报屏保"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 无参数双击 exe：只打开设置页，不启动屏保进程。
///
/// 保存成功且没有正在运行的后台实例时，保存后按新配置启动一个
/// `--background` 实例；如果已有实例，设置保存会请求它重启。
fn run_settings_only(args: &config::Args, config_path: Option<PathBuf>) -> Result<(), String> {
    let cfg = config::load(args)?;
    let path = config::display_settings_path(config_path.as_deref());
    let cache = Cache::auto(&cfg.cache_dir, cfg.cache_days, cfg.cache_naming);
    let overrides = config::SettingsOverrides::from_args(args);
    let page = if cfg.token.is_empty() {
        settings::Page::Plex
    } else {
        settings::Page::Display
    };
    let outcome = settings::run_modal(path.clone(), cache, overrides, page)?;
    if outcome.saved && !outcome.restart_requested {
        launch_background(path.as_deref())?;
    }
    Ok(())
}

fn launch_background(config_path: Option<&Path>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("获取当前 exe 路径失败: {err}"))?;
    let mut command = Command::new(exe);
    command.arg("--background");
    if let Some(path) = config_path {
        command.arg("--config").arg(path);
    }
    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("启动后台实例失败: {err}"))
}

fn relaunch_current() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("获取当前 exe 路径失败: {err}"))?;
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        // 旧快捷键/旧启动方式没有参数；现在无参数表示打开设置，重启时必须显式后台。
        args.push(OsString::from("--background"));
    }
    Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("重启程序失败: {err}"))
}

fn main() {
    console::attach_parent();
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
    if let Some(seconds) = args.diagnose_foreground {
        platform::diagnose_foreground(seconds);
        return;
    }

    let mut config_path = config::locate_config(args.config.as_deref());

    // --help / --list / --self-test / --snapshot 是工具命令，不能被单实例逻辑拦住。
    // 只有真正要开窗口的模式才检查是否已有实例在跑。
    let utility = args.list || args.self_test.is_some() || args.snapshot.is_some();
    // 双击/计划任务启动时 Windows 会给一个独立控制台；把它释放掉，避免平时
    // 常驻时挂着一个黑色终端。从 cmd/PowerShell 启动时控制台是共用的，保留输出。
    let console_state = if utility {
        console::ConsoleState::Shared
    } else {
        console::hide_if_private()
    };
    // 控制台不可见时，致命错误改用 MessageBox，避免程序一闪而过没有任何提示。
    let popup_errors = !matches!(console_state, console::ConsoleState::Shared);
    if std::env::var_os("PPS_DEBUG").is_some() {
        log_info!("控制台状态: {console_state:?}");
    }
    // 无参数双击：只打开设置页。后台运行必须显式 --background（开机自启用它），
    // 否则无参数启动会变成设置页，无法区分“配置”和“常驻”。
    if !utility && !args.background && std::env::args_os().len() == 1 {
        log_info!("无参数启动：打开设置页");
        if let Err(err) = run_settings_only(&args, config_path.clone()) {
            fatal_error(&format!("设置启动失败: {err}"), 1, popup_errors);
        }
        return;
    }
    let instance = if utility {
        None
    } else {
        match trigger::acquire() {
            Ok(trigger::Startup::First(guard)) => {
                timer::begin();
                Some(guard)
            }
            Ok(trigger::Startup::Duplicate { notified }) => {
                if notified {
                    log_info!("已经有一个实例在跑，已通知它立刻开始");
                } else {
                    log_info!("已经有一个实例在跑，但启动通知发送失败");
                }
                return;
            }
            Err(err) => {
                fatal_error(&format!("单实例初始化失败: {err}"), 1, popup_errors);
            }
        }
    };
    let trigger_event = instance.as_ref().and_then(|guard| guard.event());

    let mut cfg = match config::load(&args) {
        Ok(cfg) => cfg,
        Err(err) => {
            fatal_error(&format!("配置错误: {err}"), 2, popup_errors);
        }
    };
    let settings_overrides = config::SettingsOverrides::from_args(&args);
    if cfg.token.is_empty() && !utility && !args.background {
        log_warn!("未配置 Plex Token，先打开首次设置");
        let preview_cache = Cache::auto(&cfg.cache_dir, cfg.cache_days, cfg.cache_naming);
        let settings_path = config::display_settings_path(config_path.as_deref());
        if let Err(err) = settings::run_modal(
            settings_path,
            preview_cache,
            settings_overrides.clone(),
            settings::Page::Plex,
        ) {
            log_warn!("首次设置窗口打开失败: {err}");
        }
        config_path = config::locate_config(args.config.as_deref());
        cfg = match config::load(&args) {
            Ok(cfg) => cfg,
            Err(err) => fatal_error(&format!("配置错误: {err}"), 2, popup_errors),
        };
    }
    if cfg.token.is_empty() {
        fatal_error(
            "缺少 Plex Token。请设置环境变量 PLEX_TOKEN，或在 config.json 中填写 plex_token。",
            1,
            popup_errors,
        );
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
                fatal_error(&err, 1, popup_errors);
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
            let kind = if s.kind == "movie" {
                "电影"
            } else {
                "剧集"
            };
            println!("  {}. {}  [{}]  key={}", i + 1, s.title, kind, s.key);
        }
        return;
    }

    let mut cache = match &cfg.cache_location {
        CacheLocation::Explicit(dir) => match Cache::open(dir, cfg.cache_days, cfg.cache_naming) {
            Ok(cache) => cache,
            Err(err) => {
                fatal_error(&err, 2, popup_errors);
            }
        },
        CacheLocation::Default | CacheLocation::Auto => {
            Cache::auto(&cfg.cache_dir, cfg.cache_days, cfg.cache_naming)
        }
    };
    // 让后续托盘菜单/日志显示实际使用的目录（auto 可能回退到别处）。
    if cache.is_disk() {
        cfg.cache_dir = cache.dir.clone();
    }
    cache.set_max_bytes(cfg.cache_max_mb.saturating_mul(1024 * 1024));
    cache.maintain();
    // 保存完整媒体库列表，离线时才能把 --library 的编号/标题解析回库。
    cache.remember_sections(&sections);

    if selected.is_empty() {
        let offline = cache.offline_items(&wanted);
        if offline.is_empty() {
            let waiting =
                cfg.watch && !args.preview && args.self_test.is_none() && args.snapshot.is_none();
            if waiting {
                log_warn!("Plex 暂不可用，本地也没有可离线播放的缓存；常驻等待，后台会自动重连");
            } else {
                fatal_error(
                    &format!(
                        "无法连接 Plex，本地也没有可离线播放的缓存: {}",
                        cache.dir.display()
                    ),
                    1,
                    popup_errors,
                );
            }
        } else {
            log_warn!(
                "Plex 暂不可用，先用本地缓存播放 {} 张；后台会自动重连",
                offline.len()
            );
        }
    }

    if let Some(count) = args.self_test {
        run_self_test(&cfg, client, cache, selected, wanted, status_handle, count);
        return;
    }
    if let Some(path) = &args.snapshot {
        run_snapshot(&cfg, client, cache, selected, wanted, status_handle, path);
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
    log_info!(
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
        settings_overrides,
    );
    let outcome = app.run(trigger_event);
    timer::end();
    // 重启前先释放单实例锁，否则新进程会被旧实例拦住。
    drop(instance);
    match outcome {
        Err(err) => fatal_error(&err, 1, popup_errors),
        Ok(RunOutcome::Exit) => {}
        Ok(RunOutcome::Restart) => {
            if let Err(err) = relaunch_current() {
                fatal_error(&err, 1, popup_errors);
            }
        }
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
    let mut source = app::Source::new(
        client,
        cache,
        sections,
        size,
        wanted,
        status,
        cfg.display_layout,
    );

    println!(
        "自检开始：{count} 张，屏幕 {}x{}，布局 {}",
        size.0,
        size.1,
        cfg.display_layout.label()
    );
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
                    slide.title()
                );
            }
            Err(err) => {
                log_error!("自检失败: {err}");
                std::process::exit(1);
            }
        }
    }
    println!(
        "平均 {:.0} ms/张；转场 {:.1} 秒（{}），停留 {:.1} 秒",
        total / count as f64 * 1000.0,
        cfg.transition_seconds,
        cfg.transition.label(),
        cfg.display_seconds
    );
}

fn run_snapshot(
    cfg: &Config,
    client: Client,
    cache: Cache,
    sections: Vec<Section>,
    wanted: Vec<String>,
    status: status::Shared,
    path: &str,
) {
    let size = (1920u32, 1080u32);
    let mut source = app::Source::new(
        client,
        cache,
        sections,
        size,
        wanted,
        status,
        cfg.display_layout,
    );
    let slide = match source.next() {
        Ok(s) => s,
        Err(err) => {
            log_error!("取图失败: {err}");
            std::process::exit(1);
        }
    };

    let mut buffer = vec![0u32; (size.0 * size.1) as usize];
    render::draw_frame(&slide.frame, 0.0, (1.0, 0.0), &mut buffer, true);
    match render::save_png(&buffer, size.0, size.1, Path::new(path)) {
        Ok(()) => println!(
            "已导出渲染结果: {path} （{}，{}）",
            slide.title(),
            cfg.display_layout.label()
        ),
        Err(err) => {
            log_error!("{err}");
            std::process::exit(1);
        }
    }
}

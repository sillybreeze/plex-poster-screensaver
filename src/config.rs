//! 命令行参数与 config.json 解析。
//!
//! 优先级与 Python 版一致：命令行 > 环境变量 > config.json > 内置默认值。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};

use crate::logging::{log_info, log_warn};

pub const APP_NAME: &str = "PlexPosterScreensaver";
pub const DEFAULT_URL: &str = "http://127.0.0.1:32400";

/// 封面缓存的文件名格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Naming {
    /// `标题_1479.jpg`：一眼能认出来，靠 ratingKey 保证不会互相覆盖
    #[default]
    Title,
    /// `1479.jpg`：只按 ratingKey，名字最短
    Key,
}

impl Naming {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "title" | "标题" => Some(Self::Title),
            "key" | "id" | "编号" => Some(Self::Key),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Key => "key",
        }
    }
}

/// 全局热键，比如 Ctrl+Alt+P。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: HOT_KEY_MODIFIERS,
    pub vk: u32,
}

impl Hotkey {
    /// 默认值：Ctrl+Alt+P。
    pub fn default_hotkey() -> Self {
        Self {
            mods: HOT_KEY_MODIFIERS(MOD_CONTROL.0 | MOD_ALT.0),
            vk: 'P' as u32,
        }
    }

    /// 解析 `Ctrl+Alt+P` 这种写法；不认识的键返回 None。
    pub fn parse(raw: &str) -> Option<Self> {
        let mut mods = 0u32;
        let mut vk = None;
        for part in raw.split('+') {
            let token = part.trim();
            if token.is_empty() {
                continue;
            }
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods |= MOD_CONTROL.0,
                "alt" => mods |= MOD_ALT.0,
                "shift" => mods |= MOD_SHIFT.0,
                "win" | "super" | "meta" => mods |= MOD_WIN.0,
                other => {
                    if vk.is_some() {
                        return None;
                    }
                    vk = Some(key_code(other)?);
                }
            }
        }
        Some(Self {
            mods: HOT_KEY_MODIFIERS(mods),
            vk: vk?,
        })
    }

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.mods.0 & MOD_CONTROL.0 != 0 {
            parts.push("Ctrl".to_string());
        }
        if self.mods.0 & MOD_ALT.0 != 0 {
            parts.push("Alt".to_string());
        }
        if self.mods.0 & MOD_SHIFT.0 != 0 {
            parts.push("Shift".to_string());
        }
        if self.mods.0 & MOD_WIN.0 != 0 {
            parts.push("Win".to_string());
        }
        parts.push(key_label(self.vk));
        parts.join("+")
    }
}

fn key_code(token: &str) -> Option<u32> {
    let upper = token.to_ascii_uppercase();
    let mut chars = upper.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    if let Some(num) = upper.strip_prefix('F') {
        if let Ok(n) = num.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1);
            }
        }
    }
    match upper.as_str() {
        "SPACE" => Some(0x20),
        "TAB" => Some(0x09),
        "ENTER" | "RETURN" => Some(0x0D),
        "ESC" | "ESCAPE" => Some(0x1B),
        "HOME" => Some(0x24),
        "END" => Some(0x23),
        "PGUP" | "PAGEUP" => Some(0x21),
        "PGDN" | "PAGEDOWN" => Some(0x22),
        "INS" | "INSERT" => Some(0x2D),
        "DEL" | "DELETE" => Some(0x2E),
        "LEFT" => Some(0x25),
        "UP" => Some(0x26),
        "RIGHT" => Some(0x27),
        "DOWN" => Some(0x28),
        _ => None,
    }
}

fn key_label(vk: u32) -> String {
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        return char::from_u32(vk)
            .map(|c| c.to_string())
            .unwrap_or_default();
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x70 + 1);
    }
    match vk {
        0x20 => "Space".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x1B => "Esc".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "PgUp".to_string(),
        0x22 => "PgDn".to_string(),
        0x2D => "Ins".to_string(),
        0x2E => "Del".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        other => format!("VK{other:02X}"),
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub token: String,
    pub libraries: Vec<String>,
    pub display_seconds: f64,
    pub fade_seconds: f64,
    pub cache_dir: PathBuf,
    /// 缓存文件用哪种命名
    pub cache_naming: Naming,
    /// cache_dir 是配置文件/命令行显式给的（不是默认值）
    pub cache_dir_explicit: bool,
    /// 显式要求自动挑选（--cache-dir auto）
    pub cache_dir_auto: bool,
    pub cache_days: i64,
    /// 缓存容量上限（MB）；0 表示不限
    pub cache_max_mb: u64,
    pub verify_tls: bool,
    pub transcode: bool,
    pub timeout: u64,
    pub ken_burns: bool,
    pub motion_fps: u32,
    /// 常驻等待：空闲到点后再全屏，而不是一启动就全屏
    pub watch: bool,
    /// 触发屏保需要的空闲秒数
    pub idle_seconds: f64,
    /// 只把鼠标算作活动（默认键盘也算）
    pub mouse_only: bool,
    /// 立刻开始的全局热键；None 表示关掉
    pub hotkey: Option<Hotkey>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.to_string(),
            token: String::new(),
            libraries: Vec::new(),
            display_seconds: 10.0,
            fade_seconds: 1.2,
            cache_dir: app_dir().join("covers"),
            cache_naming: Naming::default(),
            cache_dir_explicit: false,
            cache_dir_auto: false,
            cache_days: 30,
            cache_max_mb: 2048,
            verify_tls: true,
            transcode: true,
            timeout: 10,
            ken_burns: true,
            motion_fps: 30,
            watch: false,
            idle_seconds: 300.0,
            mouse_only: false,
            hotkey: Some(Hotkey::default_hotkey()),
        }
    }
}

/// `%LOCALAPPDATA%\PlexPosterScreensaver`
pub fn app_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .or_else(|| env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

/// 可执行文件所在目录（便于把 config.json 放在 exe 旁边）。
pub fn exe_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Default)]
pub struct Args {
    pub url: Option<String>,
    pub token: Option<String>,
    pub library: Vec<String>,
    pub list: bool,
    pub preview: bool,
    pub display: Option<f64>,
    pub fade: Option<f64>,
    pub motion_fps: Option<u32>,
    pub no_motion: bool,
    pub no_transcode: bool,
    pub insecure: bool,
    pub cache_dir: Option<String>,
    pub cache_naming: Option<Naming>,
    pub cache_days: Option<i64>,
    pub cache_max_mb: Option<u64>,
    pub config: Option<String>,
    pub self_test: Option<usize>,
    pub snapshot: Option<String>,
    pub watch: bool,
    pub now: bool,
    pub idle: Option<f64>,
    pub mouse_only: bool,
    pub hotkey: Option<Hotkey>,
    pub no_hotkey: bool,
    pub verbose: bool,
    pub help: bool,
}

pub const HELP: &str = "\
plex-poster-screensaver (Rust)

用法:
  plex-poster-screensaver [选项]

选项:
  --url <地址>          Plex 地址，默认 http://127.0.0.1:32400
  --token <令牌>        Plex Token（也可用环境变量 PLEX_TOKEN）
  --library <编号|名称> 选择媒体库，可重复
  --list                列出媒体库后退出
  --display <秒>        每张停留时间，默认 10
  --fade <秒>           淡入淡出时长，默认 1.2，0 表示关闭
  --no-motion           关闭 Ken Burns 缓慢推近（轮播继续）
  --motion-fps <帧率>   动效帧率，默认 30
  --cache-dir <目录>    封面缓存位置（写不进去会直接报错）
  --cache-dir auto      自动挑一个可写目录
  --cache-days <天数>   缓存保留天数，默认 30
  --cache-max-mb <MB>   缓存容量上限，默认 2048；0 表示不限
  --cache-naming <方式> 缓存文件名：title=标题_编号（默认），key=只留编号
  --no-transcode        不使用服务端缩图
  --insecure            HTTPS 跳过证书校验
  --preview             以 1280x720 窗口预览
  --watch               常驻等待：空闲到点后自动全屏，有输入就收起
  --now                 立刻全屏，不等空闲（覆盖配置里的 watch）
  --idle <分钟>         空闲多久触发，默认 5（可写小数，如 0.1 = 6 秒）
  --mouse-only          只把鼠标当活动，键盘不算（默认和系统屏保一致：键鼠都算）
  --hotkey <组合键>     立刻开始的全局热键，默认 Ctrl+Alt+P
  --no-hotkey           停用全局热键
  --self-test <N>       无界面自检 N 张后退出
  --snapshot <PNG>      渲染一张并保存后退出
  --config <路径>       指定配置文件
  -v, --verbose         输出调试日志
  -h, --help            显示帮助
";

pub fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} 缺少参数值"));
        match arg.as_str() {
            "--url" => args.url = Some(value("--url")?),
            "--token" => args.token = Some(value("--token")?),
            "--library" => args.library.push(value("--library")?),
            "--list" => args.list = true,
            "--preview" => args.preview = true,
            "--display" => args.display = Some(parse_num(&value("--display")?)?),
            "--fade" => args.fade = Some(parse_num(&value("--fade")?)?),
            "--motion-fps" => args.motion_fps = Some(parse_num(&value("--motion-fps")?)?),
            "--no-motion" => args.no_motion = true,
            "--no-transcode" => args.no_transcode = true,
            "--insecure" => args.insecure = true,
            "--cache-dir" => args.cache_dir = Some(value("--cache-dir")?),
            "--cache-days" => args.cache_days = Some(parse_num(&value("--cache-days")?)?),
            "--cache-max-mb" => args.cache_max_mb = Some(parse_num(&value("--cache-max-mb")?)?),
            "--cache-naming" => {
                let raw = value("--cache-naming")?;
                args.cache_naming = Some(
                    Naming::parse(&raw)
                        .ok_or_else(|| format!("无效的命名方式: {raw}（title|key）"))?,
                );
            }
            "--config" => args.config = Some(value("--config")?),
            "--watch" => args.watch = true,
            "--now" => args.now = true,
            "--idle" => args.idle = Some(parse_num(&value("--idle")?)?),
            "--mouse-only" => args.mouse_only = true,
            "--no-hotkey" => args.no_hotkey = true,
            "--hotkey" => {
                let raw = value("--hotkey")?;
                args.hotkey = Some(Hotkey::parse(&raw).ok_or_else(|| {
                    format!("无效的热键: {raw}（例：Ctrl+Alt+P、Ctrl+Shift+F9）")
                })?);
            }
            "--self-test" => args.self_test = Some(parse_num(&value("--self-test")?)?),
            "--snapshot" => args.snapshot = Some(value("--snapshot")?),
            "-v" | "--verbose" => args.verbose = true,
            "-h" | "--help" => args.help = true,
            other => return Err(format!("未知参数: {other}")),
        }
    }
    Ok(args)
}

fn parse_num<T: std::str::FromStr>(raw: &str) -> Result<T, String> {
    raw.parse::<T>().map_err(|_| format!("无效的数值: {raw}"))
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    plex_url: Option<String>,
    plex_token: Option<String>,
    libraries: Option<Vec<String>>,
    display_seconds: Option<f64>,
    fade_seconds: Option<f64>,
    cache_dir: Option<String>,
    cache_naming: Option<String>,
    cache_days: Option<i64>,
    cache_max_mb: Option<u64>,
    verify_tls: Option<bool>,
    transcode: Option<bool>,
    timeout: Option<u64>,
    ken_burns: Option<bool>,
    motion_fps: Option<u32>,
    watch: Option<bool>,
    idle_minutes: Option<f64>,
    mouse_only: Option<bool>,
    hotkey: Option<String>,
}

pub fn locate_config(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("PLEX_SCREENSAVER_CONFIG") {
        return Some(PathBuf::from(path));
    }
    [exe_dir().join("config.json"), app_dir().join("config.json")]
        .into_iter()
        .find(|candidate| candidate.is_file())
}

pub fn load(args: &Args) -> Result<Config, String> {
    let mut cfg = Config::default();

    // 命令行 / 环境变量显式指定的配置文件：读不到或解析失败要明确报错，
    // 不能一边忽略错误一边用默认值继续跑，否则很容易以为配置已经生效。
    let explicit = args.config.is_some() || env::var_os("PLEX_SCREENSAVER_CONFIG").is_some();

    if let Some(path) = locate_config(args.config.as_deref()) {
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<FileConfig>(&text) {
                Ok(file) => {
                    let base = config_base_dir(&path);
                    if let Some(v) = file.plex_url {
                        cfg.url = v;
                    }
                    if let Some(v) = file.plex_token {
                        cfg.token = v;
                    }
                    if let Some(v) = file.libraries {
                        cfg.libraries = v;
                    }
                    if let Some(v) = file.display_seconds {
                        cfg.display_seconds = v;
                    }
                    if let Some(v) = file.fade_seconds {
                        cfg.fade_seconds = v;
                    }
                    if let Some(v) = file.cache_dir {
                        if v.eq_ignore_ascii_case("auto") {
                            cfg.cache_dir_auto = true;
                        } else {
                            cfg.cache_dir_explicit = true;
                            // 相对路径以配置文件所在目录为基准，而不是进程 CWD。
                            // 否则手动启动和开机自启可能写到两个不同位置。
                            cfg.cache_dir = resolve_path(&base, &v);
                        }
                    }
                    if let Some(v) = &file.cache_naming {
                        match Naming::parse(v) {
                            Some(n) => cfg.cache_naming = n,
                            None => {
                                log_warn!("config.json 里的 cache_naming 无效（title|key）: {v}")
                            }
                        }
                    }
                    if let Some(v) = file.cache_days {
                        cfg.cache_days = v;
                    }
                    if let Some(v) = file.cache_max_mb {
                        cfg.cache_max_mb = v;
                    }
                    if let Some(v) = file.verify_tls {
                        cfg.verify_tls = v;
                    }
                    if let Some(v) = file.transcode {
                        cfg.transcode = v;
                    }
                    if let Some(v) = file.timeout {
                        cfg.timeout = v;
                    }
                    if let Some(v) = file.ken_burns {
                        cfg.ken_burns = v;
                    }
                    if let Some(v) = file.motion_fps {
                        cfg.motion_fps = v;
                    }
                    if let Some(v) = file.watch {
                        cfg.watch = v;
                    }
                    if let Some(v) = file.idle_minutes {
                        cfg.idle_seconds = (v * 60.0).max(5.0);
                    }
                    if let Some(v) = file.mouse_only {
                        cfg.mouse_only = v;
                    }
                    if let Some(v) = &file.hotkey {
                        let v = v.trim();
                        if v.is_empty()
                            || v.eq_ignore_ascii_case("none")
                            || v.eq_ignore_ascii_case("off")
                        {
                            cfg.hotkey = None;
                        } else {
                            match Hotkey::parse(v) {
                                Some(h) => cfg.hotkey = Some(h),
                                None => log_warn!("config.json 里的 hotkey 无效，已忽略: {v}"),
                            }
                        }
                    }
                    log_info!("已加载配置: {}", path.display());
                }
                Err(err) => {
                    if explicit {
                        return Err(format!("配置文件解析失败 {}: {err}", path.display()));
                    }
                    log_warn!("配置文件解析失败，已忽略 {}: {err}", path.display());
                }
            },
            Err(err) => {
                if explicit {
                    return Err(format!("无法读取配置文件 {}: {err}", path.display()));
                }
                log_warn!("无法读取配置 {}: {err}", path.display());
            }
        }
    }

    if let Ok(v) = env::var("PLEX_URL") {
        cfg.url = v;
    }
    if let Ok(v) = env::var("PLEX_TOKEN") {
        cfg.token = v;
    }

    if let Some(v) = &args.url {
        cfg.url = v.clone();
    }
    if let Some(v) = &args.token {
        cfg.token = v.clone();
    }
    if let Some(v) = args.display {
        cfg.display_seconds = v.max(0.5);
    }
    if let Some(v) = args.fade {
        cfg.fade_seconds = v.max(0.0);
    }
    if let Some(v) = args.motion_fps {
        cfg.motion_fps = v.min(120);
    }
    if args.no_motion {
        cfg.ken_burns = false;
    }
    if args.watch {
        cfg.watch = true;
    }
    if args.now {
        cfg.watch = false;
    }
    if args.mouse_only {
        cfg.mouse_only = true;
    }
    if let Some(h) = args.hotkey {
        cfg.hotkey = Some(h);
    }
    if args.no_hotkey {
        cfg.hotkey = None;
    }
    if let Some(v) = args.idle {
        cfg.idle_seconds = (v * 60.0).max(5.0);
    }
    if args.no_transcode {
        cfg.transcode = false;
    }
    if args.insecure {
        cfg.verify_tls = false;
    }
    if let Some(v) = &args.cache_dir {
        if v.eq_ignore_ascii_case("auto") {
            cfg.cache_dir_auto = true;
        } else {
            cfg.cache_dir_explicit = true;
            cfg.cache_dir = PathBuf::from(v);
        }
    }
    if let Some(n) = args.cache_naming {
        cfg.cache_naming = n;
    }
    if let Some(v) = args.cache_days {
        cfg.cache_days = v.max(0);
    }
    if let Some(v) = args.cache_max_mb {
        cfg.cache_max_mb = v;
    }

    cfg.url = cfg.url.trim_end_matches('/').to_string();
    Ok(cfg)
}

/// 配置文件所在目录；相对路径的 `cache_dir` 以它为基准。
fn config_base_dir(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().unwrap_or_default().join(path)
    };
    absolute
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env::current_dir().unwrap_or_default())
}

fn resolve_path(base: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_hotkey() {
        let parsed = Hotkey::parse("Ctrl+Alt+P").expect("默认热键应该能解析");
        let default = Hotkey::default_hotkey();
        assert_eq!(parsed.vk, default.vk);
        assert_eq!(parsed.mods.0, default.mods.0);
        assert_eq!(parsed.label(), "Ctrl+Alt+P");
    }

    #[test]
    fn parses_other_combinations() {
        assert_eq!(
            Hotkey::parse("ctrl+shift+f9").unwrap().label(),
            "Ctrl+Shift+F9"
        );
        assert_eq!(Hotkey::parse("Win+Space").unwrap().label(), "Win+Space");
        assert_eq!(Hotkey::parse(" Alt + A ").unwrap().label(), "Alt+A");
    }

    #[test]
    fn rejects_nonsense() {
        assert!(Hotkey::parse("Ctrl+Nope").is_none());
        assert!(Hotkey::parse("Ctrl+A+B").is_none());
        assert!(Hotkey::parse("").is_none());
    }

    #[test]
    fn relative_cache_dir_uses_config_directory() {
        let base = Path::new("C:/configs");
        assert_eq!(resolve_path(base, "covers"), base.join("covers"));
        assert_eq!(
            resolve_path(base, "C:/other/covers"),
            PathBuf::from("C:/other/covers")
        );
    }
}

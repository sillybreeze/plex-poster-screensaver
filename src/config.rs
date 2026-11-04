//! 命令行参数与 config.json 解析。
//!
//! 优先级与 Python 版一致：命令行 > 环境变量 > config.json > 内置默认值。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// 缓存位置策略。三态互斥，避免“显式目录 + auto”同时为真。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLocation {
    /// 没有显式配置：从默认位置开始自动挑选可写目录。
    Default,
    /// 显式要求自动挑选（--cache-dir auto / config 里的 auto）。
    Auto,
    /// 严格使用指定目录；不可写就报错。
    Explicit(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub token: String,
    pub libraries: Vec<String>,
    pub display_seconds: f64,
    pub fade_seconds: f64,
    pub cache_dir: PathBuf,
    pub cache_location: CacheLocation,
    /// 缓存文件用哪种命名
    pub cache_naming: Naming,
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
            cache_location: CacheLocation::Default,
            cache_naming: Naming::default(),
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
  --display <秒>        每张停留时间，默认 10（0.1–3600）
  --fade <秒>           淡入淡出时长，默认 1.2（0–60），0 表示关闭
  --no-motion           关闭 Ken Burns 缓慢推近（轮播继续）
  --motion-fps <帧率>   动效帧率，默认 30（1–120）
  --cache-dir <目录>    封面缓存位置（写不进去会直接报错）
  --cache-dir auto      自动挑一个可写目录
  --cache-days <天数>   缓存保留天数，默认 30（0–36500，0 表示不限）
  --cache-max-mb <MB>   缓存容量上限，默认 2048；0 表示不限
  --cache-naming <方式> 缓存文件名：title=标题_编号（默认），key=只留编号
  --no-transcode        不使用服务端缩图
  --insecure            HTTPS 跳过证书校验
  --preview             以 1280x720 窗口预览
  --watch               常驻等待：空闲到点后自动全屏，有输入就收起
  --now                 立刻全屏，不等空闲（覆盖配置里的 watch）
  --idle <分钟>         空闲多久触发，默认 5（0.0833–1440，即 5 秒–24 小时）
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
    let mut file_cache_dir: Option<String> = None;
    let mut file_cache_base: Option<PathBuf> = None;

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
                        file_cache_dir = Some(v);
                        file_cache_base = Some(base);
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
                        cfg.idle_seconds = v * 60.0;
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
        cfg.display_seconds = v;
    }
    if let Some(v) = args.fade {
        cfg.fade_seconds = v;
    }
    if let Some(v) = args.motion_fps {
        cfg.motion_fps = v;
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
        cfg.idle_seconds = v * 60.0;
    }
    if args.no_transcode {
        cfg.transcode = false;
    }
    if args.insecure {
        cfg.verify_tls = false;
    }
    if let Some(n) = args.cache_naming {
        cfg.cache_naming = n;
    }
    if let Some(v) = args.cache_days {
        cfg.cache_days = v;
    }
    if let Some(v) = args.cache_max_mb {
        cfg.cache_max_mb = v;
    }

    let default_cache = cfg.cache_dir.clone();
    let file_base = file_cache_base.as_deref().unwrap_or_else(|| Path::new("."));
    let (location, dir) = choose_cache_dir(
        &default_cache,
        file_cache_dir.as_deref(),
        args.cache_dir.as_deref(),
        file_base,
    );
    cfg.cache_location = location;
    cfg.cache_dir = dir;

    cfg.url = cfg.url.trim_end_matches('/').to_string();
    validate(&cfg)?;
    Ok(cfg)
}

/// 配置合并完成后统一校验；错误信息带字段名，便于定位命令行或 config.json。
fn validate(cfg: &Config) -> Result<(), String> {
    check_seconds(
        "display_seconds（--display）",
        cfg.display_seconds,
        0.1,
        3600.0,
    )?;
    check_seconds("fade_seconds（--fade）", cfg.fade_seconds, 0.0, 60.0)?;
    check_seconds(
        "idle_seconds（--idle / idle_minutes）",
        cfg.idle_seconds,
        5.0,
        86_400.0,
    )?;

    if !(1..=600).contains(&cfg.timeout) {
        return Err(format!(
            "timeout 超出允许范围 1..=600 秒，当前为 {}",
            cfg.timeout
        ));
    }
    if !(0..=36_500).contains(&cfg.cache_days) {
        return Err(format!(
            "cache_days 超出允许范围 0..=36500 天，当前为 {}",
            cfg.cache_days
        ));
    }
    if !(1..=120).contains(&cfg.motion_fps) {
        return Err(format!(
            "motion_fps 超出允许范围 1..=120，当前为 {}",
            cfg.motion_fps
        ));
    }
    Ok(())
}

/// 校验有限性、范围和到 `Duration` 的可转换性，全部通过后上层不再有 panic 风险。
fn check_seconds(name: &str, value: f64, min: f64, max: f64) -> Result<(), String> {
    if !value.is_finite() {
        return Err(format!("{name} 必须是有限数值，当前为 {value}"));
    }
    if value < min || value > max {
        return Err(format!(
            "{name} 超出允许范围 {min}..={max} 秒，当前为 {value}"
        ));
    }
    Duration::try_from_secs_f64(value)
        .map_err(|err| format!("{name} 无法转换为时间间隔: {err}"))?;
    Ok(())
}

/// 决定缓存位置：CLI 覆盖配置文件，三态互斥。
///
/// 文件里的相对路径以配置文件目录为基准；CLI 的相对路径以进程 CWD 为基准。
/// CLI 指定 `auto` 时会清掉文件里的显式目录偏好，重新从默认位置开始自动挑选。
fn choose_cache_dir(
    default_dir: &Path,
    file: Option<&str>,
    cli: Option<&str>,
    file_base: &Path,
) -> (CacheLocation, PathBuf) {
    let mut location = CacheLocation::Default;
    let mut dir = default_dir.to_path_buf();

    if let Some(raw) = file {
        if raw.eq_ignore_ascii_case("auto") {
            location = CacheLocation::Auto;
        } else {
            let path = resolve_path(file_base, raw);
            location = CacheLocation::Explicit(path.clone());
            dir = path;
        }
    }

    if let Some(raw) = cli {
        if raw.eq_ignore_ascii_case("auto") {
            location = CacheLocation::Auto;
            dir = default_dir.to_path_buf();
        } else {
            let path = PathBuf::from(raw);
            location = CacheLocation::Explicit(path.clone());
            dir = path;
        }
    }

    (location, dir)
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

    #[test]
    fn cache_location_cli_overrides_file_strategy() {
        let default = Path::new("C:/default/covers");
        let base = Path::new("C:/configs");

        // 文件 auto -> CLI 显式目录：CLI 必须获胜，且不会被 auto 标签带回自动回退。
        let (location, dir) = choose_cache_dir(default, Some("auto"), Some("D:/explicit"), base);
        assert_eq!(
            location,
            CacheLocation::Explicit(PathBuf::from("D:/explicit"))
        );
        assert_eq!(dir, PathBuf::from("D:/explicit"));

        // 文件显式目录 -> CLI auto：清掉文件里的显式偏好，从默认位置自动挑选。
        let (location, dir) = choose_cache_dir(default, Some("covers"), Some("auto"), base);
        assert_eq!(location, CacheLocation::Auto);
        assert_eq!(dir, default.to_path_buf());

        // 文件相对路径按配置文件目录解析。
        let (location, dir) = choose_cache_dir(default, Some("covers"), None, base);
        assert_eq!(location, CacheLocation::Explicit(base.join("covers")));
        assert_eq!(dir, base.join("covers"));

        // CLI 相对路径按进程 CWD 解析（这里只验证保留相对形式）。
        let (location, dir) = choose_cache_dir(default, None, Some("relative"), base);
        assert_eq!(location, CacheLocation::Explicit(PathBuf::from("relative")));
        assert_eq!(dir, PathBuf::from("relative"));

        // 两边都没配置：默认策略。
        let (location, dir) = choose_cache_dir(default, None, None, base);
        assert_eq!(location, CacheLocation::Default);
        assert_eq!(dir, default.to_path_buf());
    }

    #[test]
    fn rejects_non_finite_and_out_of_range_values() {
        fn rejected(field: &str, cfg: Config) {
            let err = validate(&cfg).expect_err("应拒绝无效配置");
            assert!(
                err.contains(field),
                "错误信息应包含字段名 {field}，实际为 {err}"
            );
        }

        rejected(
            "display_seconds",
            Config {
                display_seconds: f64::INFINITY,
                ..Default::default()
            },
        );
        rejected(
            "display_seconds",
            Config {
                display_seconds: f64::NAN,
                ..Default::default()
            },
        );
        rejected(
            "display_seconds",
            Config {
                display_seconds: -1.0,
                ..Default::default()
            },
        );
        rejected(
            "display_seconds",
            Config {
                display_seconds: 0.0,
                ..Default::default()
            },
        );
        rejected(
            "display_seconds",
            Config {
                display_seconds: 1e308,
                ..Default::default()
            },
        );

        rejected(
            "fade_seconds",
            Config {
                fade_seconds: f64::INFINITY,
                ..Default::default()
            },
        );
        rejected(
            "fade_seconds",
            Config {
                fade_seconds: -0.1,
                ..Default::default()
            },
        );
        rejected(
            "fade_seconds",
            Config {
                fade_seconds: 60.1,
                ..Default::default()
            },
        );

        rejected(
            "idle_seconds",
            Config {
                idle_seconds: f64::INFINITY,
                ..Default::default()
            },
        );
        rejected(
            "idle_seconds",
            Config {
                idle_seconds: f64::NAN,
                ..Default::default()
            },
        );
        rejected(
            "idle_seconds",
            Config {
                idle_seconds: 4.9,
                ..Default::default()
            },
        );
        rejected(
            "idle_seconds",
            Config {
                idle_seconds: 86_401.0,
                ..Default::default()
            },
        );

        rejected(
            "timeout",
            Config {
                timeout: 0,
                ..Default::default()
            },
        );
        rejected(
            "timeout",
            Config {
                timeout: 601,
                ..Default::default()
            },
        );
        rejected(
            "cache_days",
            Config {
                cache_days: -1,
                ..Default::default()
            },
        );
        rejected(
            "cache_days",
            Config {
                cache_days: 36_501,
                ..Default::default()
            },
        );
        rejected(
            "motion_fps",
            Config {
                motion_fps: 0,
                ..Default::default()
            },
        );
        rejected(
            "motion_fps",
            Config {
                motion_fps: 121,
                ..Default::default()
            },
        );
    }

    #[test]
    fn accepts_valid_boundaries_and_zero_fade() {
        let mut cfg = Config::default();
        cfg.display_seconds = 0.1;
        cfg.fade_seconds = 0.0;
        cfg.idle_seconds = 5.0;
        cfg.timeout = 1;
        cfg.cache_days = 0;
        cfg.motion_fps = 1;
        assert!(
            validate(&cfg).is_ok(),
            "最小边界应合法: {:?}",
            validate(&cfg)
        );

        cfg.display_seconds = 3600.0;
        cfg.fade_seconds = 60.0;
        cfg.idle_seconds = 86_400.0;
        cfg.timeout = 600;
        cfg.cache_days = 36_500;
        cfg.motion_fps = 120;
        assert!(
            validate(&cfg).is_ok(),
            "最大边界应合法: {:?}",
            validate(&cfg)
        );

        // 负零按 0 处理，淡入淡出允许关闭。
        cfg.fade_seconds = -0.0;
        assert!(validate(&cfg).is_ok());
    }
}

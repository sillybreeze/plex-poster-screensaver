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

/// 显示布局：单张海报或 6 张 3×2 海报墙。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayLayout {
    #[default]
    Single,
    Wall,
}

impl DisplayLayout {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "single" | "one" | "单张" => Some(Self::Single),
            "wall" | "grid" | "海报墙" => Some(Self::Wall),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Wall => "wall",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Single => "单张海报",
            Self::Wall => "3×2 海报墙",
        }
    }
}

/// 每次更换画面时的切换效果。方向值描述画面从哪一侧进入中央。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transition {
    #[default]
    Fade,
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
    Random,
}

impl Transition {
    pub fn parse(raw: &str) -> Option<Self> {
        // 同时接受 slide_left / slideleft 这类写法，配置里少踩格式坑。
        let token = raw.trim().to_ascii_lowercase().replace('_', "-");
        match token.as_str() {
            "fade" | "crossfade" => Some(Self::Fade),
            "slide-left" | "slideleft" | "left" => Some(Self::SlideLeft),
            "slide-right" | "slideright" | "right" => Some(Self::SlideRight),
            "slide-up" | "slideup" | "up" => Some(Self::SlideUp),
            "slide-down" | "slidedown" | "down" => Some(Self::SlideDown),
            "random" => Some(Self::Random),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fade => "fade",
            Self::SlideLeft => "slide-left",
            Self::SlideRight => "slide-right",
            Self::SlideUp => "slide-up",
            Self::SlideDown => "slide-down",
            Self::Random => "random",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fade => "淡化",
            Self::SlideLeft => "从左侧滑入",
            Self::SlideRight => "从右侧滑入",
            Self::SlideUp => "从上方滑入",
            Self::SlideDown => "从下方滑入",
            Self::Random => "随机方向",
        }
    }
}

/// 动效模式。`Static` 优先级最高：关闭切换动画和缩放，但仍按间隔轮播。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionMode {
    #[default]
    Animated,
    Static,
}

impl MotionMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "animated" | "animate" | "动画" => Some(Self::Animated),
            "static" | "still" | "静止" => Some(Self::Static),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Animated => "animated",
            Self::Static => "static",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Animated => "动画",
            Self::Static => "静止",
        }
    }
}

/// 转场时长的默认值、允许范围与旧参数的 0 值兼容规则。
pub const DEFAULT_TRANSITION_SECONDS: f64 = 0.6;
pub const MIN_TRANSITION_SECONDS: f64 = 0.1;
pub const MAX_TRANSITION_SECONDS: f64 = 3.0;

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
    /// 画面切换时长；0 表示不做转场（兼容旧 `--fade 0`）。
    pub transition_seconds: f64,
    pub display_layout: DisplayLayout,
    pub transition: Transition,
    pub motion_mode: MotionMode,
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
            transition_seconds: DEFAULT_TRANSITION_SECONDS,
            display_layout: DisplayLayout::default(),
            transition: Transition::default(),
            motion_mode: MotionMode::default(),
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
    /// 等待 N 秒后打印前台窗口诊断信息（T08 排查用）。
    pub diagnose_foreground: Option<u32>,
    pub preview: bool,
    pub display: Option<f64>,
    pub fade: Option<f64>,
    pub layout: Option<DisplayLayout>,
    pub transition: Option<Transition>,
    pub motion: Option<MotionMode>,
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
  --diagnose-foreground <秒>  等待 N 秒后打印前台窗口诊断信息（全屏识别排查）
  --display <秒>        每张停留时间，默认 10（0.1–3600）
  --layout <布局>       single=单张海报（默认），wall=6 张 3×2 海报墙
  --transition <效果>   fade=淡化（默认）、slide-left/slide-right/slide-up/slide-down
                        从对应一侧滑入、random=随机方向
  --fade <秒>           转场时长，默认 0.6（0 或 0.1–3，0 表示关闭）；旧参数名，
                        同时控制方向滑入时长
  --motion <模式>       animated=带动效（默认），static=静止轮播（无转场、无缩放）
  --no-motion           等同 --motion static（保留兼容）
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
    parse_args_from(env::args().skip(1))
}

fn parse_args_from(raw: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = raw.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} 缺少参数值"));
        match arg.as_str() {
            "--url" => args.url = Some(value("--url")?),
            "--token" => args.token = Some(value("--token")?),
            "--library" => args.library.push(value("--library")?),
            "--list" => args.list = true,
            "--diagnose-foreground" => {
                let seconds = parse_num::<u32>(&value("--diagnose-foreground")?)?;
                if seconds > 600 {
                    return Err("--diagnose-foreground 最多等待 600 秒".to_string());
                }
                args.diagnose_foreground = Some(seconds);
            }
            "--preview" => args.preview = true,
            "--display" => args.display = Some(parse_num(&value("--display")?)?),
            "--fade" => args.fade = Some(parse_num(&value("--fade")?)?),
            "--layout" => {
                let raw = value("--layout")?;
                args.layout = Some(
                    DisplayLayout::parse(&raw)
                        .ok_or_else(|| format!("--layout 的参数无效: {raw}（single|wall）"))?,
                );
            }
            "--transition" => {
                let raw = value("--transition")?;
                args.transition = Some(Transition::parse(&raw).ok_or_else(|| {
                    format!(
                        "--transition 的参数无效: {raw}（fade|slide-left|slide-right|slide-up|slide-down|random）"
                    )
                })?);
            }
            "--motion" => {
                let raw = value("--motion")?;
                args.motion = Some(
                    MotionMode::parse(&raw)
                        .ok_or_else(|| format!("--motion 的参数无效: {raw}（animated|static）"))?,
                );
            }
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
    /// 旧字段名；作为 `transition_seconds` 的兼容别名读取。
    fade_seconds: Option<f64>,
    transition_seconds: Option<f64>,
    display_layout: Option<String>,
    transition: Option<String>,
    motion_mode: Option<String>,
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
                    if let Some(v) = &file.cache_dir {
                        file_cache_dir = Some(v.clone());
                        file_cache_base = Some(base);
                    }
                    apply_file(&mut cfg, &file)?;
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

    apply_args(&mut cfg, args);

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

/// 把 config.json 的字段合并进配置。
///
/// 旧字段 `fade_seconds` 继续作为 `transition_seconds` 的别名；同时出现时新键优先。
/// 未知的布局/转场/动效值直接报错，错误信息带 JSON 键名。
fn apply_file(cfg: &mut Config, file: &FileConfig) -> Result<(), String> {
    if let Some(v) = &file.plex_url {
        cfg.url = v.clone();
    }
    if let Some(v) = &file.plex_token {
        cfg.token = v.clone();
    }
    if let Some(v) = &file.libraries {
        cfg.libraries = v.clone();
    }
    if let Some(v) = file.display_seconds {
        cfg.display_seconds = v;
    }
    if let Some(v) = file.fade_seconds {
        cfg.transition_seconds = v;
    }
    if let Some(v) = file.transition_seconds {
        cfg.transition_seconds = v;
    }
    if let Some(raw) = &file.display_layout {
        cfg.display_layout = DisplayLayout::parse(raw)
            .ok_or_else(|| format!("config.json 里的 display_layout 无效（single|wall）: {raw}"))?;
    }
    if let Some(raw) = &file.transition {
        cfg.transition = Transition::parse(raw).ok_or_else(|| {
            format!(
                "config.json 里的 transition 无效（fade|slide-left|slide-right|slide-up|slide-down|random）: {raw}"
            )
        })?;
    }
    if let Some(raw) = &file.motion_mode {
        cfg.motion_mode = MotionMode::parse(raw).ok_or_else(|| {
            format!("config.json 里的 motion_mode 无效（animated|static）: {raw}")
        })?;
    }
    if let Some(v) = &file.cache_naming {
        match Naming::parse(v) {
            Some(n) => cfg.cache_naming = n,
            None => log_warn!("config.json 里的 cache_naming 无效（title|key）: {v}"),
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
        if v.is_empty() || v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("off") {
            cfg.hotkey = None;
        } else {
            match Hotkey::parse(v) {
                Some(h) => cfg.hotkey = Some(h),
                None => log_warn!("config.json 里的 hotkey 无效，已忽略: {v}"),
            }
        }
    }
    Ok(())
}

/// 命令行覆盖；优先级固定为 CLI > 环境/文件 > 默认值，且与参数出现顺序无关。
///
/// `--no-motion` 是 `--motion static` 的快捷方式：两者都要求静止；
/// 若同时给了 `--motion animated`，按“静止优先”处理而不是看谁写在后面。
fn apply_args(cfg: &mut Config, args: &Args) {
    if let Some(v) = &args.url {
        cfg.url = v.clone();
    }
    if let Some(v) = &args.token {
        cfg.token = v.clone();
    }
    if let Some(v) = args.display {
        cfg.display_seconds = v;
    }
    // --fade 是旧参数名，现在只作为转场时长的别名。
    if let Some(v) = args.fade {
        cfg.transition_seconds = v;
    }
    if let Some(v) = args.layout {
        cfg.display_layout = v;
    }
    if let Some(v) = args.transition {
        cfg.transition = v;
    }
    if let Some(v) = args.motion {
        cfg.motion_mode = v;
    }
    if args.no_motion {
        cfg.motion_mode = MotionMode::Static;
    }
    if let Some(v) = args.motion_fps {
        cfg.motion_fps = v;
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
}

/// 配置合并完成后统一校验；错误信息带字段名，便于定位命令行或 config.json。
fn validate(cfg: &Config) -> Result<(), String> {
    check_seconds(
        "display_seconds（--display）",
        cfg.display_seconds,
        0.1,
        3600.0,
    )?;
    check_transition_seconds(cfg.transition_seconds)?;
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

/// 校验转场时长：有限，且为旧的 0（立即切换）或新的 0.1–3.0 秒范围。
///
/// 0 是 `--fade 0` 的历史语义，保留下来避免已有启动脚本失效；
/// 其余 0.1 以下的值仍按越界处理，避免出现一闪而过、比一帧还短的转场。
fn check_transition_seconds(value: f64) -> Result<(), String> {
    if !value.is_finite() {
        return Err(format!(
            "transition_seconds（--fade） 必须是有限数值，当前为 {value}"
        ));
    }
    let legacy_off = value == 0.0;
    if !legacy_off && !(MIN_TRANSITION_SECONDS..=MAX_TRANSITION_SECONDS).contains(&value) {
        return Err(format!(
            "transition_seconds（--fade） 超出允许范围 {MIN_TRANSITION_SECONDS}..={MAX_TRANSITION_SECONDS} 秒（0 表示关闭），当前为 {value}"
        ));
    }
    Duration::try_from_secs_f64(value)
        .map_err(|err| format!("transition_seconds（--fade） 无法转换为时间间隔: {err}"))?;
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
            "transition_seconds",
            Config {
                transition_seconds: f64::INFINITY,
                ..Default::default()
            },
        );
        rejected(
            "transition_seconds",
            Config {
                transition_seconds: f64::NAN,
                ..Default::default()
            },
        );
        rejected(
            "transition_seconds",
            Config {
                transition_seconds: -0.1,
                ..Default::default()
            },
        );
        rejected(
            "transition_seconds",
            Config {
                transition_seconds: 0.05,
                ..Default::default()
            },
        );
        rejected(
            "transition_seconds",
            Config {
                transition_seconds: 3.1,
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
        let min = Config {
            display_seconds: 0.1,
            transition_seconds: 0.0,
            idle_seconds: 5.0,
            timeout: 1,
            cache_days: 0,
            motion_fps: 1,
            ..Default::default()
        };
        assert!(
            validate(&min).is_ok(),
            "最小边界应合法: {:?}",
            validate(&min)
        );

        let max = Config {
            display_seconds: 3600.0,
            transition_seconds: 3.0,
            idle_seconds: 86_400.0,
            timeout: 600,
            cache_days: 36_500,
            motion_fps: 120,
            ..Default::default()
        };
        assert!(
            validate(&max).is_ok(),
            "最大边界应合法: {:?}",
            validate(&max)
        );

        // 负零按 0 处理：旧的 --fade 0 仍然表示立即切换。
        let negative_zero = Config {
            transition_seconds: -0.0,
            ..Default::default()
        };
        assert!(validate(&negative_zero).is_ok());

        // 0.1 秒是新的最小时长。
        let minimum = Config {
            transition_seconds: 0.1,
            ..Default::default()
        };
        assert!(validate(&minimum).is_ok());
    }

    #[test]
    fn parses_display_layout_values() {
        assert_eq!(DisplayLayout::parse("single"), Some(DisplayLayout::Single));
        assert_eq!(DisplayLayout::parse(" WALL "), Some(DisplayLayout::Wall));
        assert_eq!(DisplayLayout::parse("海报墙"), Some(DisplayLayout::Wall));
        assert_eq!(DisplayLayout::parse("mosaic"), None);
        assert_eq!(DisplayLayout::Single.as_str(), "single");
        assert_eq!(DisplayLayout::Wall.as_str(), "wall");
    }

    #[test]
    fn parses_transition_values() {
        let cases = [
            ("fade", Transition::Fade),
            ("slide-left", Transition::SlideLeft),
            ("slide_left", Transition::SlideLeft),
            ("SLIDE-RIGHT", Transition::SlideRight),
            ("slide-up", Transition::SlideUp),
            ("slide-down", Transition::SlideDown),
            ("random", Transition::Random),
        ];
        for (raw, expected) in cases {
            assert_eq!(Transition::parse(raw), Some(expected), "解析 {raw}");
        }
        assert_eq!(Transition::parse("wipe"), None);

        // 序列化字符串要能再解析回来。
        for value in [
            Transition::Fade,
            Transition::SlideLeft,
            Transition::SlideRight,
            Transition::SlideUp,
            Transition::SlideDown,
            Transition::Random,
        ] {
            assert_eq!(Transition::parse(value.as_str()), Some(value));
        }
    }

    #[test]
    fn parses_motion_mode_values() {
        assert_eq!(MotionMode::parse("animated"), Some(MotionMode::Animated));
        assert_eq!(MotionMode::parse("STATIC"), Some(MotionMode::Static));
        assert_eq!(MotionMode::parse("静止"), Some(MotionMode::Static));
        assert_eq!(MotionMode::parse("slow"), None);
        assert_eq!(MotionMode::Animated.as_str(), "animated");
        assert_eq!(MotionMode::Static.as_str(), "static");
    }

    #[test]
    fn defaults_match_documented_display_settings() {
        let cfg = Config::default();
        assert_eq!(cfg.display_layout, DisplayLayout::Single);
        assert_eq!(cfg.transition, Transition::Fade);
        assert!((cfg.transition_seconds - 0.6).abs() < 1e-9);
        assert_eq!(cfg.motion_mode, MotionMode::Animated);
        assert!(cfg.ken_burns);
    }

    #[test]
    fn legacy_file_config_keeps_old_display_behavior() {
        // 升级前的配置：没有新字段，fade_seconds=1.2，关掉 Ken Burns。
        let file: FileConfig =
            serde_json::from_str(r#"{"display_seconds":10,"fade_seconds":1.2,"ken_burns":false}"#)
                .unwrap();
        let mut cfg = Config::default();
        apply_file(&mut cfg, &file).unwrap();

        assert_eq!(cfg.display_layout, DisplayLayout::Single);
        assert_eq!(cfg.transition, Transition::Fade);
        assert_eq!(cfg.motion_mode, MotionMode::Animated);
        assert!(!cfg.ken_burns, "旧配置的 ken_burns=false 仍然只关缩放");
        assert!((cfg.transition_seconds - 1.2).abs() < 1e-9);
        assert!(validate(&cfg).is_ok());

        // 完全没有新字段的旧配置同样维持默认显示。
        let empty: FileConfig = serde_json::from_str(r#"{"plex_url":"http://x"}"#).unwrap();
        let mut cfg = Config::default();
        apply_file(&mut cfg, &empty).unwrap();
        assert_eq!(cfg.display_layout, DisplayLayout::Single);
        assert_eq!(cfg.transition, Transition::Fade);
        assert!((cfg.transition_seconds - DEFAULT_TRANSITION_SECONDS).abs() < 1e-9);
    }

    #[test]
    fn transition_seconds_overrides_legacy_fade_seconds() {
        let file: FileConfig =
            serde_json::from_str(r#"{"fade_seconds":1.5,"transition_seconds":0.4}"#).unwrap();
        let mut cfg = Config::default();
        apply_file(&mut cfg, &file).unwrap();
        assert!((cfg.transition_seconds - 0.4).abs() < 1e-9);
    }

    #[test]
    fn file_display_settings_are_parsed() {
        let file: FileConfig = serde_json::from_str(
            r#"{"display_layout":"wall","transition":"slide-down","motion_mode":"static","ken_burns":true,"transition_seconds":2.5}"#,
        )
        .unwrap();
        let mut cfg = Config::default();
        apply_file(&mut cfg, &file).unwrap();
        assert_eq!(cfg.display_layout, DisplayLayout::Wall);
        assert_eq!(cfg.transition, Transition::SlideDown);
        assert_eq!(cfg.motion_mode, MotionMode::Static);
        assert!(cfg.ken_burns);
        assert!((cfg.transition_seconds - 2.5).abs() < 1e-9);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn invalid_enum_in_file_reports_key_name() {
        for (json, key) in [
            (r#"{"display_layout":"mosaic"}"#, "display_layout"),
            (r#"{"transition":"wipe"}"#, "transition"),
            (r#"{"motion_mode":"slow"}"#, "motion_mode"),
        ] {
            let file: FileConfig = serde_json::from_str(json).unwrap();
            let mut cfg = Config::default();
            let err = apply_file(&mut cfg, &file).expect_err("未知枚举值必须报错");
            assert!(
                err.contains(key),
                "错误信息应包含 JSON 键 {key}，实际为 {err}"
            );
        }
    }

    #[test]
    fn cli_display_settings_override_file() {
        let file: FileConfig = serde_json::from_str(
            r#"{"display_layout":"single","transition":"fade","motion_mode":"animated","transition_seconds":0.6}"#,
        )
        .unwrap();
        let mut cfg = Config::default();
        apply_file(&mut cfg, &file).unwrap();

        let args = Args {
            layout: Some(DisplayLayout::Wall),
            transition: Some(Transition::SlideLeft),
            motion: Some(MotionMode::Animated),
            fade: Some(1.5),
            ..Default::default()
        };
        apply_args(&mut cfg, &args);

        assert_eq!(cfg.display_layout, DisplayLayout::Wall);
        assert_eq!(cfg.transition, Transition::SlideLeft);
        assert_eq!(cfg.motion_mode, MotionMode::Animated);
        assert!((cfg.transition_seconds - 1.5).abs() < 1e-9);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn no_motion_equals_static_regardless_of_argument_order() {
        // 先 --motion animated 后 --no-motion：静止优先。
        let args = Args {
            motion: Some(MotionMode::Animated),
            no_motion: true,
            ..Default::default()
        };
        let mut cfg = Config::default();
        apply_args(&mut cfg, &args);
        assert_eq!(cfg.motion_mode, MotionMode::Static);

        // 只有 --motion static：与 --no-motion 等价。
        let args = Args {
            motion: Some(MotionMode::Static),
            ..Default::default()
        };
        let mut cfg = Config::default();
        apply_args(&mut cfg, &args);
        assert_eq!(cfg.motion_mode, MotionMode::Static);

        // 文件配置是 static，CLI 显式 animated 可以覆盖（然后 no-motion 再拉回静止）。
        let mut cfg = Config {
            motion_mode: MotionMode::Static,
            ..Default::default()
        };
        let args = Args {
            motion: Some(MotionMode::Animated),
            ..Default::default()
        };
        apply_args(&mut cfg, &args);
        assert_eq!(cfg.motion_mode, MotionMode::Animated);
    }

    #[test]
    fn parse_args_accepts_display_flags() {
        let args = parse_args_from(
            [
                "--layout",
                "wall",
                "--transition",
                "slide-up",
                "--motion",
                "static",
                "--fade",
                "1.5",
                "--display",
                "8",
            ]
            .map(str::to_string),
        )
        .unwrap();
        assert_eq!(args.layout, Some(DisplayLayout::Wall));
        assert_eq!(args.transition, Some(Transition::SlideUp));
        assert_eq!(args.motion, Some(MotionMode::Static));
        assert_eq!(args.fade, Some(1.5));
        assert_eq!(args.display, Some(8.0));
    }

    #[test]
    fn parse_args_reports_invalid_enum_with_flag_name() {
        for (flag, value, name) in [
            ("--layout", "mosaic", "--layout"),
            ("--transition", "wipe", "--transition"),
            ("--motion", "slow", "--motion"),
        ] {
            let err = parse_args_from([flag.to_string(), value.to_string()])
                .expect_err("未知枚举值必须报错");
            assert!(
                err.contains(name),
                "错误信息应包含参数名 {name}，实际为 {err}"
            );
        }
    }

    #[test]
    fn no_motion_is_static_alias_on_cli() {
        let args = parse_args_from(["--no-motion".to_string()]).unwrap();
        let mut cfg = Config::default();
        apply_args(&mut cfg, &args);
        assert_eq!(cfg.motion_mode, MotionMode::Static);

        // 旧行为是只关 Ken Burns，现在由静止模式统一覆盖；
        // 想只关缩放仍可在配置里写 ken_burns=false。
        assert!(cfg.ken_burns, "配置里的 ken_burns 不因 --no-motion 被改写");
    }
}

//! 命令行参数与 config.json 解析。
//!
//! 优先级与 Python 版一致：命令行 > 环境变量 > config.json > 内置默认值。

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};
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

/// 设置窗口读写的常用显示项。
///
/// 只覆盖日常调整的 6 个字段；其余高级配置保存在原 JSON 里，保存时原样保留。
#[derive(Debug, Clone, PartialEq)]
pub struct DisplaySettings {
    pub display_layout: DisplayLayout,
    pub transition: Transition,
    pub transition_seconds: f64,
    pub display_seconds: f64,
    pub motion_mode: MotionMode,
    pub ken_burns: bool,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

impl DisplaySettings {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            display_layout: cfg.display_layout,
            transition: cfg.transition,
            transition_seconds: cfg.transition_seconds,
            display_seconds: cfg.display_seconds,
            motion_mode: cfg.motion_mode,
            ken_burns: cfg.ken_burns,
        }
    }

    /// 设置窗口保存前的校验；错误信息直接显示给用户。
    pub fn validate(&self) -> Result<(), String> {
        if !self.display_seconds.is_finite() || !(0.1..=3600.0).contains(&self.display_seconds) {
            return Err("每张停留时间必须在 0.1–3600 秒之间".to_string());
        }
        if !self.transition_seconds.is_finite()
            || (self.transition_seconds != 0.0
                && !(MIN_TRANSITION_SECONDS..=MAX_TRANSITION_SECONDS)
                    .contains(&self.transition_seconds))
        {
            return Err("切换时长必须是 0（立即切换）或 0.1–3.0 秒".to_string());
        }
        Duration::try_from_secs_f64(self.display_seconds)
            .map_err(|err| format!("每张停留时间无法转换为时间间隔: {err}"))?;
        Duration::try_from_secs_f64(self.transition_seconds)
            .map_err(|err| format!("切换时长无法转换为时间间隔: {err}"))?;
        Ok(())
    }

    /// 从配置文件 JSON 读取常用项；缺失字段用默认值，旧 `fade_seconds` 仍兼容。
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let mut settings = Self::default();
        if let Some(raw) = value.get("display_layout") {
            let raw = raw
                .as_str()
                .ok_or_else(|| "config.json 里的 display_layout 必须是字符串".to_string())?;
            settings.display_layout = DisplayLayout::parse(raw).ok_or_else(|| {
                format!("config.json 里的 display_layout 无效（single|wall）: {raw}")
            })?;
        }
        if let Some(raw) = value.get("transition") {
            let raw = raw
                .as_str()
                .ok_or_else(|| "config.json 里的 transition 必须是字符串".to_string())?;
            settings.transition = Transition::parse(raw).ok_or_else(|| {
                format!(
                    "config.json 里的 transition 无效（fade|slide-left|slide-right|slide-up|slide-down|random）: {raw}"
                )
            })?;
        }
        let seconds = value
            .get("transition_seconds")
            .or_else(|| value.get("fade_seconds"));
        if let Some(raw) = seconds {
            settings.transition_seconds = raw
                .as_f64()
                .ok_or_else(|| "config.json 里的 transition_seconds 必须是数值".to_string())?;
        }
        if let Some(raw) = value.get("display_seconds") {
            settings.display_seconds = raw
                .as_f64()
                .ok_or_else(|| "config.json 里的 display_seconds 必须是数值".to_string())?;
        }
        if let Some(raw) = value.get("motion_mode") {
            let raw = raw
                .as_str()
                .ok_or_else(|| "config.json 里的 motion_mode 必须是字符串".to_string())?;
            settings.motion_mode = MotionMode::parse(raw).ok_or_else(|| {
                format!("config.json 里的 motion_mode 无效（animated|static）: {raw}")
            })?;
        }
        if let Some(raw) = value.get("ken_burns") {
            settings.ken_burns = raw
                .as_bool()
                .ok_or_else(|| "config.json 里的 ken_burns 必须是布尔值".to_string())?;
        }
        Ok(settings)
    }

    /// 只覆盖常用键，保留 `plex_url`、缓存、热键等高级字段；旧别名会被删除。
    pub fn apply_to_json(&self, value: &mut Value) -> Result<(), String> {
        let map = value
            .as_object_mut()
            .ok_or_else(|| "配置文件根节点必须是 JSON 对象".to_string())?;
        map.insert(
            "display_layout".to_string(),
            Value::String(self.display_layout.as_str().to_string()),
        );
        map.insert(
            "transition".to_string(),
            Value::String(self.transition.as_str().to_string()),
        );
        map.insert(
            "transition_seconds".to_string(),
            Value::from(self.transition_seconds),
        );
        map.insert(
            "display_seconds".to_string(),
            Value::from(self.display_seconds),
        );
        map.insert(
            "motion_mode".to_string(),
            Value::String(self.motion_mode.as_str().to_string()),
        );
        map.insert("ken_burns".to_string(), Value::Bool(self.ken_burns));
        map.remove("fade_seconds");
        Ok(())
    }
}

/// 设置窗口编辑的完整持久化配置。
///
/// 与运行期 [`Config`] 分开：这里保留文件里的原始语义（例如 `cache_dir: "auto"`、
/// 空 Token、空热键），保存时再写回 JSON，未知高级字段原样保留。
#[derive(Debug, Clone, PartialEq)]
pub struct EditableSettings {
    pub plex_url: String,
    pub plex_token: String,
    pub libraries: Vec<String>,
    pub verify_tls: bool,
    pub timeout: u64,
    pub display: DisplaySettings,
    pub motion_fps: u32,
    pub watch: bool,
    pub idle_minutes: f64,
    pub mouse_only: bool,
    /// 空字符串表示停用热键。
    pub hotkey: String,
    pub cache_dir: String,
    /// true 时保存成 `"auto"`，忽略 `cache_dir`。
    pub cache_auto: bool,
    pub cache_naming: Naming,
    pub cache_days: i64,
    pub cache_max_mb: u64,
    pub transcode: bool,
}

impl Default for EditableSettings {
    fn default() -> Self {
        let cfg = Config::default();
        let display = DisplaySettings::from_config(&cfg);
        Self {
            plex_url: cfg.url,
            plex_token: cfg.token,
            libraries: Vec::new(),
            verify_tls: cfg.verify_tls,
            timeout: cfg.timeout,
            display,
            motion_fps: cfg.motion_fps,
            watch: cfg.watch,
            idle_minutes: cfg.idle_seconds / 60.0,
            mouse_only: cfg.mouse_only,
            hotkey: cfg.hotkey.map(|hotkey| hotkey.label()).unwrap_or_default(),
            cache_dir: cfg.cache_dir.to_string_lossy().into_owned(),
            // 没有显式 cache_dir 时保持“自动挑选可写目录”的语义，避免
            // GUI 保存后把默认路径固化成可能不可写的绝对路径。
            cache_auto: true,
            cache_naming: cfg.cache_naming,
            cache_days: cfg.cache_days,
            cache_max_mb: cfg.cache_max_mb,
            transcode: cfg.transcode,
        }
    }
}

impl EditableSettings {
    /// 从 config.json 读取全部持久字段；缺失项用默认值，路径按配置文件目录解析。
    pub fn from_json(value: &Value, base: &Path) -> Result<Self, String> {
        let display = DisplaySettings::from_json(value)?;
        let mut settings = Self {
            display,
            ..Self::default()
        };

        if let Some(raw) = value.get("plex_url") {
            settings.plex_url = raw
                .as_str()
                .ok_or_else(|| "config.json 里的 plex_url 必须是字符串".to_string())?
                .to_string();
        }
        if let Some(raw) = value.get("plex_token") {
            settings.plex_token = raw
                .as_str()
                .ok_or_else(|| "config.json 里的 plex_token 必须是字符串".to_string())?
                .to_string();
        }
        if let Some(raw) = value.get("libraries") {
            let items = raw
                .as_array()
                .ok_or_else(|| "config.json 里的 libraries 必须是数组".to_string())?;
            let mut libraries = Vec::with_capacity(items.len());
            for item in items {
                let title = item
                    .as_str()
                    .ok_or_else(|| "config.json 里的 libraries 必须都是字符串".to_string())?;
                libraries.push(title.to_string());
            }
            settings.libraries = libraries;
        }
        if let Some(raw) = value.get("verify_tls") {
            settings.verify_tls = raw
                .as_bool()
                .ok_or_else(|| "config.json 里的 verify_tls 必须是布尔值".to_string())?;
        }
        if let Some(raw) = value.get("timeout") {
            settings.timeout = raw
                .as_u64()
                .ok_or_else(|| "config.json 里的 timeout 必须是整数".to_string())?;
        }
        if let Some(raw) = value.get("motion_fps") {
            settings.motion_fps = raw
                .as_u64()
                .ok_or_else(|| "config.json 里的 motion_fps 必须是整数".to_string())?
                .try_into()
                .map_err(|_| "config.json 里的 motion_fps 超出范围".to_string())?;
        }
        if let Some(raw) = value.get("watch") {
            settings.watch = raw
                .as_bool()
                .ok_or_else(|| "config.json 里的 watch 必须是布尔值".to_string())?;
        }
        if let Some(raw) = value.get("idle_minutes") {
            settings.idle_minutes = raw
                .as_f64()
                .ok_or_else(|| "config.json 里的 idle_minutes 必须是数值".to_string())?;
        }
        if let Some(raw) = value.get("mouse_only") {
            settings.mouse_only = raw
                .as_bool()
                .ok_or_else(|| "config.json 里的 mouse_only 必须是布尔值".to_string())?;
        }
        if let Some(raw) = value.get("hotkey") {
            let raw = raw
                .as_str()
                .ok_or_else(|| "config.json 里的 hotkey 必须是字符串".to_string())?;
            settings.hotkey = match raw.trim() {
                "" | "none" | "off" => String::new(),
                other => other.to_string(),
            };
        }
        if let Some(raw) = value.get("cache_dir") {
            let raw = raw
                .as_str()
                .ok_or_else(|| "config.json 里的 cache_dir 必须是字符串".to_string())?;
            let raw = raw.trim();
            if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
                settings.cache_auto = true;
                settings.cache_dir = Config::default().cache_dir.to_string_lossy().into_owned();
            } else {
                settings.cache_auto = false;
                settings.cache_dir = resolve_path(base, raw).to_string_lossy().into_owned();
            }
        }
        if let Some(raw) = value.get("cache_naming") {
            let raw = raw
                .as_str()
                .ok_or_else(|| "config.json 里的 cache_naming 必须是字符串".to_string())?;
            settings.cache_naming = Naming::parse(raw)
                .ok_or_else(|| format!("config.json 里的 cache_naming 无效（title|key）: {raw}"))?;
        }
        if let Some(raw) = value.get("cache_days") {
            settings.cache_days = raw
                .as_i64()
                .ok_or_else(|| "config.json 里的 cache_days 必须是整数".to_string())?;
        }
        if let Some(raw) = value.get("cache_max_mb") {
            settings.cache_max_mb = raw
                .as_u64()
                .ok_or_else(|| "config.json 里的 cache_max_mb 必须是整数".to_string())?;
        }
        if let Some(raw) = value.get("transcode") {
            settings.transcode = raw
                .as_bool()
                .ok_or_else(|| "config.json 里的 transcode 必须是布尔值".to_string())?;
        }
        Ok(settings)
    }

    /// 只覆盖持久化字段，保留未知/高级 JSON 键；旧 `fade_seconds` 会被移除。
    pub fn apply_to_json(&self, value: &mut Value) -> Result<(), String> {
        self.display.apply_to_json(value)?;
        let map = value
            .as_object_mut()
            .ok_or_else(|| "配置文件根节点必须是 JSON 对象".to_string())?;
        map.insert("plex_url".to_string(), Value::String(self.plex_url.clone()));
        map.insert(
            "plex_token".to_string(),
            Value::String(self.plex_token.clone()),
        );
        map.insert(
            "libraries".to_string(),
            Value::Array(
                self.libraries
                    .iter()
                    .map(|title| Value::String(title.clone()))
                    .collect(),
            ),
        );
        map.insert("verify_tls".to_string(), Value::Bool(self.verify_tls));
        map.insert("timeout".to_string(), Value::from(self.timeout));
        map.insert("motion_fps".to_string(), Value::from(self.motion_fps));
        map.insert("watch".to_string(), Value::Bool(self.watch));
        map.insert("idle_minutes".to_string(), Value::from(self.idle_minutes));
        map.insert("mouse_only".to_string(), Value::Bool(self.mouse_only));
        map.insert(
            "hotkey".to_string(),
            Value::String(if self.hotkey.trim().is_empty() {
                "none".to_string()
            } else {
                self.hotkey.trim().to_string()
            }),
        );
        map.insert(
            "cache_dir".to_string(),
            Value::String(if self.cache_auto {
                "auto".to_string()
            } else {
                self.cache_dir.trim().to_string()
            }),
        );
        map.insert(
            "cache_naming".to_string(),
            Value::String(self.cache_naming.as_str().to_string()),
        );
        map.insert("cache_days".to_string(), Value::from(self.cache_days));
        map.insert("cache_max_mb".to_string(), Value::from(self.cache_max_mb));
        map.insert("transcode".to_string(), Value::Bool(self.transcode));
        Ok(())
    }

    /// 校验全部持久字段。`require_token=false` 用于 Token 由环境变量提供的场景。
    pub fn validate(&self, require_token: bool) -> Result<(), String> {
        self.display.validate()?;
        let url = self.plex_url.trim();
        if url.is_empty() {
            return Err("Plex 服务器地址不能为空".to_string());
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("Plex 服务器地址必须以 http:// 或 https:// 开头".to_string());
        }
        if require_token && self.plex_token.trim().is_empty() {
            return Err("Plex Token 不能为空".to_string());
        }
        if !(1..=600).contains(&self.timeout) {
            return Err("连接超时必须在 1–600 秒之间".to_string());
        }
        let min_idle = 5.0 / 60.0;
        if !self.idle_minutes.is_finite() || !(min_idle..=1440.0).contains(&self.idle_minutes) {
            return Err("空闲触发时间必须在 5 秒–24 小时之间".to_string());
        }
        if !(1..=120).contains(&self.motion_fps) {
            return Err("动效帧率必须在 1–120 之间".to_string());
        }
        if !(0..=36_500).contains(&self.cache_days) {
            return Err("缓存保留天数必须在 0–36500 天之间".to_string());
        }
        if !self.cache_auto && self.cache_dir.trim().is_empty() {
            return Err("缓存目录不能为空；也可以勾选“自动选择”".to_string());
        }
        let hotkey = self.hotkey.trim();
        if !hotkey.is_empty()
            && !matches!(hotkey, "none" | "off")
            && Hotkey::parse(hotkey).is_none()
        {
            return Err(format!("热键格式无效: {hotkey}（例：Ctrl+Alt+P）"));
        }
        Ok(())
    }
}

/// 当前运行被哪些启动参数/环境变量覆盖；设置窗口只做提示，不改变保存目标。
#[derive(Debug, Clone, Default)]
pub struct SettingsOverrides {
    pub plex_url: bool,
    pub plex_token: bool,
    pub libraries: bool,
    pub display_seconds: bool,
    pub transition: bool,
    pub transition_seconds: bool,
    pub display_layout: bool,
    pub motion_mode: bool,
    pub motion_fps: bool,
    pub watch: bool,
    pub idle: bool,
    pub mouse_only: bool,
    pub hotkey: bool,
    pub verify_tls: bool,
    pub transcode: bool,
    pub cache_dir: bool,
    pub cache_naming: bool,
    pub cache_days: bool,
    pub cache_max_mb: bool,
}

impl SettingsOverrides {
    pub fn from_args(args: &Args) -> Self {
        let env_url = env::var_os("PLEX_URL").is_some();
        let env_token = env::var_os("PLEX_TOKEN").is_some();
        Self {
            plex_url: args.url.is_some() || env_url,
            plex_token: args.token.is_some() || env_token,
            libraries: !args.library.is_empty(),
            display_seconds: args.display.is_some(),
            transition: args.transition.is_some(),
            transition_seconds: args.fade.is_some(),
            display_layout: args.layout.is_some(),
            motion_mode: args.motion.is_some() || args.no_motion,
            motion_fps: args.motion_fps.is_some(),
            watch: args.watch || args.now,
            idle: args.idle.is_some(),
            mouse_only: args.mouse_only,
            hotkey: args.hotkey.is_some() || args.no_hotkey,
            verify_tls: args.insecure,
            transcode: args.no_transcode,
            cache_dir: args.cache_dir.is_some(),
            cache_naming: args.cache_naming.is_some(),
            cache_days: args.cache_days.is_some(),
            cache_max_mb: args.cache_max_mb.is_some(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.summary().is_empty()
    }

    pub fn summary(&self) -> String {
        let mut items = Vec::new();
        if self.plex_url {
            items.push("Plex 地址");
        }
        if self.plex_token {
            items.push("Plex Token");
        }
        if self.libraries {
            items.push("媒体库");
        }
        if self.display_layout || self.transition || self.transition_seconds {
            items.push("显示效果");
        }
        if self.display_seconds {
            items.push("停留时间");
        }
        if self.motion_mode || self.motion_fps {
            items.push("动效");
        }
        if self.watch || self.idle {
            items.push("触发方式");
        }
        if self.mouse_only {
            items.push("活动检测");
        }
        if self.hotkey {
            items.push("热键");
        }
        if self.verify_tls || self.transcode {
            items.push("Plex 高级选项");
        }
        if self.cache_dir || self.cache_naming || self.cache_days || self.cache_max_mb {
            items.push("缓存");
        }
        items.join("、")
    }
}

/// 读取设置窗口的完整配置；文件不存在时返回默认值。
pub fn load_editable_settings(path: Option<&Path>) -> Result<EditableSettings, String> {
    let Some(path) = path else {
        return Ok(EditableSettings::default());
    };
    match fs::read_to_string(path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .map_err(|err| format!("配置文件解析失败 {}: {err}", path.display()))?;
            EditableSettings::from_json(&value, &config_base_dir(path))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(EditableSettings::default()),
        Err(err) => Err(format!("无法读取配置文件 {}: {err}", path.display())),
    }
}

/// 保存完整配置；保留未知字段，写入前做范围校验（允许空 Token，由调用方决定）。
pub fn save_editable_settings(
    path: Option<&Path>,
    settings: &EditableSettings,
) -> Result<(), String> {
    settings.validate(false)?;
    let Some(path) = path else {
        return Err("没有可写的配置文件位置；可用 --config 指定一个可写路径".to_string());
    };
    save_preserving(path, |value| settings.apply_to_json(value))
}

/// 设置窗口保存的目标：已有配置文件优先，否则在可写位置新建。
pub fn display_settings_path(existing: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = existing {
        return Some(path.to_path_buf());
    }
    [exe_dir().join("config.json"), app_dir().join("config.json")]
        .into_iter()
        .find(|candidate| candidate.parent().is_some_and(probe_writable))
}

fn probe_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".settings-write-probe-{}", std::process::id()));
    let ok = fs::write(&probe, b"1").is_ok();
    let _ = fs::remove_file(probe);
    ok
}

/// 读取常用显示设置；文件不存在时返回默认值。
/// 保留给旧调用方/测试；设置中心使用 `load_editable_settings`。
#[allow(dead_code)]
pub fn load_display_settings(path: Option<&Path>) -> Result<DisplaySettings, String> {
    let Some(path) = path else {
        return Ok(DisplaySettings::default());
    };
    match fs::read_to_string(path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .map_err(|err| format!("配置文件解析失败 {}: {err}", path.display()))?;
            DisplaySettings::from_json(&value)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(DisplaySettings::default()),
        Err(err) => Err(format!("无法读取配置文件 {}: {err}", path.display())),
    }
}

/// 读取现有 JSON，应用字段变更后原子写回；未知字段原样保留。
fn save_preserving(
    path: &Path,
    apply: impl FnOnce(&mut Value) -> Result<(), String>,
) -> Result<(), String> {
    save_preserving_with(path, apply, |from, to| fs::rename(from, to))
}

fn save_preserving_with(
    path: &Path,
    apply: impl FnOnce(&mut Value) -> Result<(), String>,
    replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<(), String> {
    let mut value = if path.exists() {
        let text = fs::read_to_string(path)
            .map_err(|err| format!("无法读取配置文件 {}: {err}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|err| format!("现有配置文件解析失败，未覆盖 {}: {err}", path.display()))?
    } else {
        Value::Object(Map::new())
    };
    if !value.is_object() {
        return Err(format!(
            "配置文件根节点不是 JSON 对象，未保存: {}",
            path.display()
        ));
    }
    apply(&mut value)?;

    let text =
        serde_json::to_string_pretty(&value).map_err(|err| format!("序列化配置失败: {err}"))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("创建配置目录失败 {}: {err}", parent.display()))?;
        }
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    if let Err(err) = fs::write(&tmp, format!("{text}\n")) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("写入配置临时文件失败: {err}"));
    }
    if let Err(err) = replace(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("保存配置失败 {}: {err}", path.display()));
    }
    Ok(())
}

/// 把常用显示设置写回配置文件，保留未识别的 JSON 字段。
/// 保留给旧调用方/测试；设置中心使用 `save_editable_settings`。
#[allow(dead_code)]
pub fn save_display_settings(
    path: Option<&Path>,
    settings: &DisplaySettings,
) -> Result<(), String> {
    settings.validate()?;
    let Some(path) = path else {
        return Err("没有可写的配置文件位置；可用 --config 指定一个可写路径".to_string());
    };
    save_preserving(path, |value| settings.apply_to_json(value))
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
    /// 无界面后台运行：开机自启/重启时使用，不打开设置页。
    pub background: bool,
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

说明:
  不加参数启动会直接打开设置页；后台运行请用 --background。

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
  --background          后台运行，不打开设置页（开机自启/重启使用）
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
            "--background" => args.background = true,
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

    fn settings_scratch_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("settings-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn display_settings_defaults_match_config_defaults() {
        let defaults = DisplaySettings::default();
        let cfg = Config::default();
        assert_eq!(defaults.display_layout, cfg.display_layout);
        assert_eq!(defaults.transition, cfg.transition);
        assert_eq!(defaults.transition_seconds, cfg.transition_seconds);
        assert_eq!(defaults.display_seconds, cfg.display_seconds);
        assert_eq!(defaults.motion_mode, cfg.motion_mode);
        assert_eq!(defaults.ken_burns, cfg.ken_burns);
        assert!(defaults.validate().is_ok());
    }

    #[test]
    fn display_settings_validation_rejects_bad_ranges() {
        let short = DisplaySettings {
            display_seconds: 0.05,
            ..Default::default()
        };
        assert!(short.validate().unwrap_err().contains("停留"));

        let nan = DisplaySettings {
            display_seconds: f64::NAN,
            ..Default::default()
        };
        assert!(nan.validate().is_err());

        let slow = DisplaySettings {
            transition_seconds: 3.5,
            ..Default::default()
        };
        assert!(slow.validate().unwrap_err().contains("切换"));

        let infinite = DisplaySettings {
            transition_seconds: f64::INFINITY,
            ..Default::default()
        };
        assert!(infinite.validate().is_err());

        // 0 保留旧 --fade 0 的“立即切换”语义；0.1/3.0 是边界。
        for value in [0.0, 0.1, 3.0] {
            let settings = DisplaySettings {
                transition_seconds: value,
                ..Default::default()
            };
            assert!(settings.validate().is_ok(), "边界值 {value} 应合法");
        }
    }

    #[test]
    fn display_settings_json_reads_common_and_legacy_fields() {
        let value: Value = serde_json::from_str(
            r#"{"display_layout":"wall","transition":"slide-up","fade_seconds":1.4,"display_seconds":18,"motion_mode":"static","ken_burns":false}"#,
        )
        .unwrap();
        let settings = DisplaySettings::from_json(&value).unwrap();
        assert_eq!(settings.display_layout, DisplayLayout::Wall);
        assert_eq!(settings.transition, Transition::SlideUp);
        assert!((settings.transition_seconds - 1.4).abs() < 1e-9);
        assert!((settings.display_seconds - 18.0).abs() < 1e-9);
        assert_eq!(settings.motion_mode, MotionMode::Static);
        assert!(!settings.ken_burns);

        // 新键优先于旧 fade_seconds。
        let value: Value =
            serde_json::from_str(r#"{"fade_seconds":1.4,"transition_seconds":0.5}"#).unwrap();
        assert!(
            (DisplaySettings::from_json(&value)
                .unwrap()
                .transition_seconds
                - 0.5)
                .abs()
                < 1e-9
        );

        let value: Value = serde_json::from_str(r#"{"display_layout":"mosaic"}"#).unwrap();
        assert!(DisplaySettings::from_json(&value).is_err());
    }

    #[test]
    fn saving_display_settings_preserves_advanced_fields() {
        let dir = settings_scratch_dir("roundtrip");
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{
                "plex_url": "http://plex:32400",
                "plex_token": "secret",
                "libraries": ["电影"],
                "cache_dir": "covers",
                "cache_days": 45,
                "display_layout": "single",
                "transition": "fade",
                "fade_seconds": 1.2,
                "display_seconds": 20,
                "motion_mode": "animated",
                "ken_burns": false,
                "future_option": {"keep": true}
            }"#,
        )
        .unwrap();

        let mut settings = load_display_settings(Some(&path)).unwrap();
        assert!((settings.transition_seconds - 1.2).abs() < 1e-9);
        settings.display_layout = DisplayLayout::Wall;
        settings.transition = Transition::SlideLeft;
        settings.transition_seconds = 1.5;
        settings.display_seconds = 8.0;
        settings.motion_mode = MotionMode::Animated;
        settings.ken_burns = true;
        save_display_settings(Some(&path), &settings).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["plex_url"], "http://plex:32400");
        assert_eq!(value["plex_token"], "secret");
        assert_eq!(value["cache_dir"], "covers");
        assert_eq!(value["cache_days"], 45);
        assert_eq!(value["libraries"][0], "电影");
        assert_eq!(value["future_option"]["keep"], true);
        assert_eq!(value["display_layout"], "wall");
        assert_eq!(value["transition"], "slide-left");
        assert_eq!(value["display_seconds"], 8.0);
        assert_eq!(value["ken_burns"], true);
        assert!(
            value.get("fade_seconds").is_none(),
            "保存新键后应移除旧别名，避免两个时长字段并存"
        );
        assert_eq!(load_display_settings(Some(&path)).unwrap(), settings);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_display_settings_creates_missing_file_and_directories() {
        let dir = settings_scratch_dir("create");
        let path = dir.join("nested").join("config.json");
        assert!(!path.exists());

        let settings = DisplaySettings {
            display_layout: DisplayLayout::Wall,
            transition: Transition::Random,
            transition_seconds: 0.8,
            display_seconds: 12.0,
            motion_mode: MotionMode::Static,
            ken_burns: false,
        };
        save_display_settings(Some(&path), &settings).unwrap();
        assert!(path.is_file());
        assert_eq!(load_display_settings(Some(&path)).unwrap(), settings);

        // 不存在的文件按默认值读取。
        let missing = dir.join("missing.json");
        assert_eq!(
            load_display_settings(Some(&missing)).unwrap(),
            DisplaySettings::default()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_settings_refuses_to_clobber_invalid_json() {
        let dir = settings_scratch_dir("invalid");
        let path = dir.join("config.json");
        fs::write(&path, "{ not valid json").unwrap();
        let err = save_display_settings(Some(&path), &DisplaySettings::default())
            .expect_err("损坏配置不能被覆盖");
        assert!(err.contains("解析失败"), "错误信息应说明解析失败: {err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not valid json");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_config_replacement_keeps_original_file() {
        let dir = settings_scratch_dir("replace-failure");
        let path = dir.join("config.json");
        let tmp = dir.join(format!(".config.json.{}.tmp", std::process::id()));
        fs::write(&path, r#"{"custom":"original"}"#).unwrap();

        let result = save_preserving_with(
            &path,
            |value| {
                value["custom"] = Value::String("replacement".to_string());
                Ok(())
            },
            |_, _| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulated replacement failure",
                ))
            },
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"custom":"original"}"#
        );
        assert!(!tmp.exists(), "失败时应清理临时文件");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_settings_path_prefers_existing_path() {
        let dir = settings_scratch_dir("path");
        let path = dir.join("custom.json");
        assert_eq!(display_settings_path(Some(&path)), Some(path.clone()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn editable_settings_round_trip_preserves_advanced_fields() {
        let raw = r#"{
            "plex_url": "http://192.168.1.2:32400",
            "plex_token": "token",
            "libraries": ["电影", "剧集"],
            "display_layout": "wall",
            "transition": "slide-left",
            "fade_seconds": 1.1,
            "transition_seconds": 0.8,
            "display_seconds": 8,
            "motion_mode": "static",
            "ken_burns": false,
            "motion_fps": 24,
            "watch": true,
            "idle_minutes": 7,
            "mouse_only": true,
            "hotkey": "Ctrl+Shift+F9",
            "cache_dir": "covers",
            "cache_naming": "key",
            "cache_days": 45,
            "cache_max_mb": 1024,
            "verify_tls": false,
            "timeout": 20,
            "transcode": false,
            "custom_advanced": {"keep": true}
        }"#;
        let mut value: Value = serde_json::from_str(raw).unwrap();
        let base = Path::new("C:/base");
        let settings = EditableSettings::from_json(&value, base).unwrap();

        assert_eq!(settings.plex_url, "http://192.168.1.2:32400");
        assert_eq!(
            settings.libraries,
            vec!["电影".to_string(), "剧集".to_string()]
        );
        assert_eq!(settings.display.display_layout, DisplayLayout::Wall);
        assert_eq!(settings.display.transition, Transition::SlideLeft);
        assert!((settings.display.transition_seconds - 0.8).abs() < 1e-9);
        assert_eq!(settings.motion_fps, 24);
        assert!(settings.mouse_only);
        assert_eq!(settings.idle_minutes, 7.0);
        assert_eq!(settings.cache_naming, Naming::Key);
        assert!(!settings.cache_auto);
        assert!(settings.cache_dir.ends_with("covers"));

        settings.apply_to_json(&mut value).unwrap();
        let map = value.as_object().unwrap();
        assert!(map.get("fade_seconds").is_none(), "旧字段应被迁移并删除");
        assert!(map.get("custom_advanced").is_some(), "高级字段必须保留");
        assert_eq!(
            map.get("cache_dir").unwrap().as_str().unwrap(),
            settings.cache_dir
        );
        assert_eq!(map.get("idle_minutes").unwrap().as_f64(), Some(7.0));
        assert_eq!(map.get("hotkey").unwrap().as_str(), Some("Ctrl+Shift+F9"));
    }

    #[test]
    fn editable_settings_save_and_load_round_trip() {
        let dir = settings_scratch_dir("editable");
        let path = dir.join("config.json");
        let settings = EditableSettings {
            plex_token: "secret".to_string(),
            timeout: 22,
            cache_days: 12,
            cache_auto: true,
            display: DisplaySettings {
                display_seconds: 6.0,
                ..DisplaySettings::default()
            },
            ..EditableSettings::default()
        };
        fs::write(&path, r#"{"plex_token":"secret","old_key":42}"#).unwrap();

        save_editable_settings(Some(&path), &settings).unwrap();
        let loaded = load_editable_settings(Some(&path)).unwrap();
        assert_eq!(loaded.timeout, 22);
        assert_eq!(loaded.cache_days, 12);
        assert!((loaded.display.display_seconds - 6.0).abs() < 1e-9);
        assert!(loaded.cache_auto);
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value.get("old_key").unwrap().as_u64(), Some(42));
        assert_eq!(value.get("cache_dir").unwrap().as_str(), Some("auto"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn editable_settings_validation_covers_all_ranges() {
        let base = EditableSettings::default();
        assert!(base.validate(true).is_err(), "默认没有 Token");
        assert!(base.validate(false).is_ok(), "Token 可由环境变量提供");

        let mut valid = base.clone();
        valid.plex_url = "http://127.0.0.1:32400".to_string();
        valid.plex_token = "token".to_string();
        assert!(valid.validate(true).is_ok());
        assert!(valid.validate(false).is_ok());

        let mut empty_token = valid.clone();
        empty_token.plex_token.clear();
        assert!(empty_token.validate(false).is_ok());
        assert!(empty_token.validate(true).is_err());

        for (mutate, label) in [
            (
                Box::new(|s: &mut EditableSettings| s.timeout = 0)
                    as Box<dyn Fn(&mut EditableSettings)>,
                "timeout",
            ),
            (
                Box::new(|s: &mut EditableSettings| s.idle_minutes = 0.01),
                "idle",
            ),
            (
                Box::new(|s: &mut EditableSettings| s.motion_fps = 121),
                "fps",
            ),
            (
                Box::new(|s: &mut EditableSettings| s.cache_days = 36_501),
                "days",
            ),
            (
                Box::new(|s: &mut EditableSettings| s.hotkey = "NotAKey".to_string()),
                "hotkey",
            ),
            (
                Box::new(|s: &mut EditableSettings| s.plex_url = "ftp://x".to_string()),
                "url",
            ),
        ] {
            let mut broken = valid.clone();
            mutate(&mut broken);
            assert!(broken.validate(false).is_err(), "{label} 应被拒绝");
        }
        let _ = base;
    }

    #[test]
    fn settings_overrides_summarize_launch_overrides() {
        let args = Args {
            url: Some("http://x".to_string()),
            layout: Some(DisplayLayout::Wall),
            no_transcode: true,
            ..Args::default()
        };
        let overrides = SettingsOverrides::from_args(&args);
        assert!(overrides.plex_url);
        assert!(overrides.display_layout);
        assert!(overrides.transcode);
        assert!(overrides.summary().contains("Plex 地址"));
        assert!(overrides.summary().contains("Plex 高级选项"));
    }
}

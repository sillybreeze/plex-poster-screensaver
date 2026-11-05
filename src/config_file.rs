//! config.json 的定位、读取、写回和启动覆盖链。
//!
//! 配置模型本身在 `config.rs`；本模块只处理文件、环境变量和命令行
//! 如何合并成运行期 `Config`。

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::cli::Args;
use crate::config::{
    app_dir, exe_dir, CacheLocation, Config, DisplayLayout, DisplaySettings, EditableSettings,
    Hotkey, MotionMode, Naming, Transition, MAX_TRANSITION_SECONDS, MIN_TRANSITION_SECONDS,
};
use crate::logging::{log_info, log_warn};

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

pub(crate) fn save_preserving_with(
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

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FileConfig {
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
pub(crate) fn apply_file(cfg: &mut Config, file: &FileConfig) -> Result<(), String> {
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
pub(crate) fn apply_args(cfg: &mut Config, args: &Args) {
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
pub(crate) fn validate(cfg: &Config) -> Result<(), String> {
    if !crate::config_validate::valid_url(&cfg.url) {
        return Err(format!(
            "plex_url 必须是 http:// 或 https:// 地址，当前为 {:?}",
            cfg.url
        ));
    }
    check_seconds(
        "display_seconds（--display）",
        cfg.display_seconds,
        crate::config_validate::MIN_DISPLAY_SECONDS,
        crate::config_validate::MAX_DISPLAY_SECONDS,
    )?;
    check_transition_seconds(cfg.transition_seconds)?;
    if !crate::config_validate::valid_idle_seconds(cfg.idle_seconds) {
        return Err(format!(
            "idle_seconds（--idle / idle_minutes） 超出允许范围 5..=86400 秒，当前为 {}",
            cfg.idle_seconds
        ));
    }
    Duration::try_from_secs_f64(cfg.idle_seconds).map_err(|err| {
        format!("idle_seconds（--idle / idle_minutes） 无法转换为时间间隔: {err}")
    })?;

    if !crate::config_validate::valid_timeout(cfg.timeout) {
        return Err(format!(
            "timeout 超出允许范围 1..=600 秒，当前为 {}",
            cfg.timeout
        ));
    }
    if !crate::config_validate::valid_cache_days(cfg.cache_days) {
        return Err(format!(
            "cache_days 超出允许范围 0..=36500 天，当前为 {}",
            cfg.cache_days
        ));
    }
    if !crate::config_validate::valid_motion_fps(cfg.motion_fps) {
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
    if !crate::config_validate::finite_in_range(value, min, max) {
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
    if !crate::config_validate::valid_transition_seconds(value) {
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
pub(crate) fn choose_cache_dir(
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
pub(crate) fn config_base_dir(path: &Path) -> PathBuf {
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

pub(crate) fn resolve_path(base: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

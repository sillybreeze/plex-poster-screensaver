//! 命令行参数与 config.json 解析。
//!
//! 优先级与 Python 版一致：命令行 > 环境变量 > config.json > 内置默认值。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

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
            verify_tls: true,
            transcode: true,
            timeout: 10,
            ken_burns: true,
            motion_fps: 30,
            watch: false,
            idle_seconds: 300.0,
            mouse_only: false,
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
    pub config: Option<String>,
    pub self_test: Option<usize>,
    pub snapshot: Option<String>,
    pub watch: bool,
    pub now: bool,
    pub idle: Option<f64>,
    pub mouse_only: bool,
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
  --no-motion           关闭 Ken Burns 缓慢推近
  --motion-fps <帧率>   动效帧率，默认 30
  --cache-dir <目录>    封面缓存位置（写不进去会直接报错）
  --cache-dir auto      自动挑一个可写目录
  --cache-days <天数>   缓存保留天数，默认 30
  --cache-naming <方式> 缓存文件名：title=标题_编号（默认），key=只留编号
  --no-transcode        不使用服务端缩图
  --insecure            HTTPS 跳过证书校验
  --preview             以 1280x720 窗口预览
  --watch               常驻等待：空闲到点后自动全屏，有输入就收起
  --now                 立刻全屏，不等空闲（覆盖配置里的 watch）
  --idle <分钟>         空闲多久触发，默认 5（可写小数，如 0.1 = 6 秒）
  --mouse-only          只把鼠标当活动，键盘不算（默认和系统屏保一致：键鼠都算）
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
            "--cache-naming" => {
                let raw = value("--cache-naming")?;
                args.cache_naming =
                    Some(Naming::parse(&raw).ok_or_else(|| format!("无效的命名方式: {raw}（title|key）"))?);
            }
            "--config" => args.config = Some(value("--config")?),
            "--watch" => args.watch = true,
            "--now" => args.now = true,
            "--idle" => args.idle = Some(parse_num(&value("--idle")?)?),
            "--mouse-only" => args.mouse_only = true,
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
    verify_tls: Option<bool>,
    transcode: Option<bool>,
    timeout: Option<u64>,
    ken_burns: Option<bool>,
    motion_fps: Option<u32>,
    watch: Option<bool>,
    idle_minutes: Option<f64>,
    mouse_only: Option<bool>,
}

pub fn locate_config(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("PLEX_SCREENSAVER_CONFIG") {
        return Some(PathBuf::from(path));
    }
    for candidate in [exe_dir().join("config.json"), app_dir().join("config.json")] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn load(args: &Args) -> Config {
    let mut cfg = Config::default();

    if let Some(path) = locate_config(args.config.as_deref()) {
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<FileConfig>(&text) {
                Ok(file) => {
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
                            cfg.cache_dir = PathBuf::from(v);
                        }
                    }
                    if let Some(v) = &file.cache_naming {
                        match Naming::parse(v) {
                            Some(n) => cfg.cache_naming = n,
                            None => eprintln!("config.json 里的 cache_naming 无效（title|key）: {v}"),
                        }
                    }
                    if let Some(v) = file.cache_days {
                        cfg.cache_days = v;
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
                    eprintln!("已加载配置: {}", path.display());
                }
                Err(err) => eprintln!("配置文件解析失败，已忽略 {}: {err}", path.display()),
            },
            Err(err) => eprintln!("无法读取配置 {}: {err}", path.display()),
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

    cfg.url = cfg.url.trim_end_matches('/').to_string();
    cfg
}

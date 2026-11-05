//! 命令行参数解析。只依赖配置模型，不读写配置文件。

use std::env;

use crate::config::{DisplayLayout, Hotkey, MotionMode, Naming, Transition};

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

pub(crate) fn parse_args_from(raw: impl IntoIterator<Item = String>) -> Result<Args, String> {
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

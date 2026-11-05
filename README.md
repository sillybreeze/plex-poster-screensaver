# plex-poster-screensaver

Plex 海报屏保：从媒体库随机取电影/剧集海报全屏轮播，自动提取主色做氛围光，并带 Ken Burns 轻动效。Rust + D3D11 实现。

> Windows screensaver that displays your Plex movie and TV posters full-screen, with ambient color glow and Ken Burns motion.

[![Release](https://img.shields.io/github/v/release/sillybreeze/plex-poster-screensaver?label=release)](https://github.com/sillybreeze/plex-poster-screensaver/releases/latest)

![截图](docs/screenshot.png)

## 特性

- **氛围光 + Ken Burns**：按主色生成渐变、模糊纹理和暗角；10 秒缓慢推近 3.5%
- **三种显示设置自由组合**：单张海报或 6 张 3×2 海报墙；淡化或四方向滑入切换；正常动效或完全静止轮播
- **D3D11 渲染**：缩放和双线性采样交给 GPU，每帧 CPU 约 0.05 ms；方向转场只移动图层，不重建纹理
- **后台预取**：下载、解码、合成、取色都在工作线程，主界面不卡；海报墙整面画布在 worker 里合成，主线程只上传一张纹理
- **本地缓存 + 离线播放**：`index.json` 记录媒体信息，Plex 断网时继续轮播，并每 30 秒后台重连
- **常驻等待**：`--watch` 隐藏等待，空闲到点全屏，有输入立刻收起
- **托盘控制**：查看在线/离线、缓存数量和当前海报；立即播放、暂停 1 小时、恢复、打开配置/缓存/日志、退出
- **免打扰**：全屏游戏、全屏视频或演示模式时自动暂停；锁屏、显示器关闭、休眠时停止工作，恢复后重建渲染
- **可排查**：日志写到 `%LOCALAPPDATA%\PlexPosterScreensaver\logs\app.log`，托盘菜单可直接打开
- **缓存维护**：按天数和容量上限清理，海报 thumb 版本变化时自动刷新
- **尺寸跟随**：分辨率变化时重建交换链、合成尺寸和预取线程

## 下载

[Releases](https://github.com/sillybreeze/plex-poster-screensaver/releases/latest) 提供 GitHub Actions 自动构建的 Windows x64 exe：

`plex-poster-screensaver-vX.Y.Z-windows-x64.exe`

未做代码签名，首次运行 Windows SmartScreen 可能提示，选择“仍要运行”即可。

## 快速开始

需要 Windows 10/11 和 Rust 1.75+。先按下方[配置](#配置)准备好项目根目录的 `config.json`，然后：

```powershell
cargo build --release
.\target\release\plex-poster-screensaver.exe --config config.json --list
.\target\release\plex-poster-screensaver.exe --config config.json --preview --display 3
```

`--self-test 5` 无界面取 5 张并打印耗时；`--snapshot out.png` 导出当前合成结果；两者同样要带 `--config config.json`。

## 常驻等待

```powershell
.\target\release\plex-poster-screensaver.exe --config config.json --watch
```

- `--idle <分钟>`：空闲多久触发，默认 5；可写小数（`0.1` = 6 秒），最小 5 秒
- `--mouse-only`：只把鼠标算活动；默认键盘也算
- `--hotkey <组合键>`：立刻全屏，默认 `Ctrl+Alt+P`；`--no-hotkey` 关闭
- `--now`：不等空闲，立刻全屏
- 再启动一次 exe 会通知已运行的实例立刻开始（单实例）
- 热键/再启动拉起后的前 1.2 秒是宽限期，用来吞掉按键松开产生的输入

没触发时窗口隐藏、不渲染、不 `Present`，D3D11 设备等第一次触发才创建。

### 托盘与免打扰

常驻模式启动后，通知区域会有托盘图标：

- 状态行显示在线/离线、缓存数量和当前海报；鼠标悬停也能看到
- **立即播放**、**暂停自动触发 1 小时**、**恢复自动触发**
- 打开配置文件、缓存目录、日志目录
- 退出程序

全屏游戏、全屏视频、PowerPoint 演示时自动暂停触发；锁屏、显示器关闭、系统休眠时同样暂停并停止渲染，恢复后自动重建 D3D 设备。窗口化的视频/游戏无法可靠识别，需要时用托盘的“暂停 1 小时”。

日志文件在 `%LOCALAPPDATA%\PlexPosterScreensaver\logs\app.log`，启动时若超过 5MB 会轮换成 `app.1.log`；后台运行出问题时先看这里。

### 开机自启

优先在设置中心的“启动与热键”页勾选；程序会写一个隐藏运行的启动文件夹 VBS。

手动创建快捷方式时，参数必须带 `--background`，否则双击/登录会打开设置页而不是后台运行；登录后是否先等待空闲由设置里的“常驻等待”决定：

```powershell
$startup = [Environment]::GetFolderPath('Startup')
$ws = New-Object -ComObject WScript.Shell
$lnk = $ws.CreateShortcut("$startup\Plex 海报屏保.lnk")
$lnk.TargetPath = "D:\path\to\plex-poster-screensaver.exe"
$lnk.Arguments = '--background --config "D:\path\to\config.json"'
$lnk.Save()
```

`--config` 建议显式指定：exe 默认只找同目录和 `%LOCALAPPDATA%` 下的 `config.json`。

## 配置

优先级：命令行 > 环境变量 > `config.json` > 默认值。可从示例复制一份，再填 `plex_url` 和 `plex_token`：

```powershell
Copy-Item config.example.json config.json
```

```json
{
  "plex_url": "http://192.168.1.10:32400",
  "plex_token": "你的 Plex Token",
  "libraries": ["电影", "动漫"],
  "display_seconds": 10,
  "display_layout": "single",
  "transition": "fade",
  "transition_seconds": 0.6,
  "motion_mode": "animated",
  "cache_dir": "covers",
  "cache_naming": "title",
  "cache_days": 30,
  "cache_max_mb": 2048,
  "ken_burns": true,
  "motion_fps": 30
}
```

`PLEX_URL` / `PLEX_TOKEN` 可替代配置里的地址和 Token。`cache_dir` 的相对路径按 **config.json 所在目录** 解析；命令行 `--cache-dir` 的相对路径按当前工作目录。显式 `--config` 读取或解析失败会直接报错。

### 显示模式

布局、切换效果和动效模式是三个独立开关，可以自由组合：

| 配置项 | 可选值 | 默认 | 说明 |
|---|---|---|---|
| `display_layout` | `single`、`wall` | `single` | 单张海报，或 6 张 3×2 海报墙 |
| `transition` | `fade`、`slide_left`、`slide_right`、`slide_up`、`slide_down`、`random` | `fade` | 每次换画面的效果；方向值表示画面从哪一侧滑入 |
| `transition_seconds` | 0.1–3.0 | `0.6` | 切换时长；0 表示立即切换 |
| `motion_mode` | `animated`、`static` | `animated` | `static` 关闭切换动画和缩放，但仍按 `display_seconds` 换图 |
| `ken_burns` | 布尔 | `true` | 仅动画模式下的单张布局启用缓慢推近 |

- `static` 优先级最高：即使 `transition` 不是 `fade`，也只直接切换；海报墙不缩放。
- `ken_burns: false` 只关闭缩放，淡化/滑入仍然生效。
- `random` 在每张新画面开始时选一次方向，不会在动画中途改变，也不会和上一次方向连续重复。
- 海报墙在缓存不足 6 张时用可用数量重新排网格；只剩 1 张时退回单张布局；下载失败的部分会被跳过，全部失败则保持等待/错误状态。
- 旧配置里的 `fade_seconds` 仍作为 `transition_seconds` 的别名读取；`--fade` 参数保留，同时控制方向滑入时长。

### 设置中心

托盘菜单里的 **设置…** 打开分页设置窗口，日常调整不用记 JSON 字段：

| 页面 | 内容 |
|---|---|
| 显示 | 布局、切换效果/时长、停留时间、动效模式、Ken Burns、实时预览 |
| Plex 服务器 | 地址、Token（掩码）、验证 HTTPS、超时、测试连接、媒体库勾选 |
| 启动与热键 | 常驻等待、空闲分钟数、只算鼠标、全局热键、开机自启 |
| 缓存 | 缓存目录/自动选择、命名、保留天数、容量上限、服务端缩图、统计/清理 |
| 高级 | 动效帧率、配置文件路径、打开配置文件/日志目录 |
| 日志 | `app.log` 最近 300 行，只读、可滚动、每秒刷新 |

第一次启动没有 Token 时，会先弹出 Plex 配置页：填写地址和 Token、测试连接、勾选媒体库、保存，随后自动启动后台实例，不需要手动创建 `config.json`。

Plex 测试连接在后台线程执行，不会卡住窗口；连接结果会区分成功、HTTP 错误、超时等。Token 默认掩码显示，可勾选“显示 Token”。

**保存**只覆盖界面中的持久字段，配置里未知/高级字段原样保留；旧 `fade_seconds` 会迁移成 `transition_seconds`。保存会**立即重启正在运行的后台实例**；如果是双击 exe 打开的独立设置窗口且当前没有实例，保存后会自动按新配置启动后台。命令行参数仍优先于文件，窗口顶部会提示当前被哪些启动参数/环境变量覆盖。

**开机自启**写入当前用户的“启动”文件夹（隐藏运行的 VBS，带 `--background` 和当前配置路径）；登录后按“常驻等待”设置运行，不申请管理员权限，取消勾选会删除该脚本。

**双击 exe（无参数）只打开设置页，不直接后台运行**；后台运行使用 `--background`（开机自启脚本内部就是这样启动的）。托盘菜单新增 **重启程序**。

配置文件不存在时，会在 exe 目录或 `%LOCALAPPDATA%\PlexPosterScreensaver\` 下新建；两处都不可写时保存按钮会禁用。高级页可以直接打开配置文件；不常见的部署/自动化场景仍可继续手写 JSON。

### 控制台与日志

后台启动（`--background`、开机自启或计划任务）时，如果控制台是本进程独占的，程序会主动释放它，不再一直挂着一个黑色终端窗口。从 `cmd` / PowerShell 启动时会保留共用的控制台，`--help`、`--list`、`--self-test`、`--snapshot` 等命令的输出仍然可见。

日志始终写到 `%LOCALAPPDATA%\PlexPosterScreensaver\logs\app.log`（超过 5MB 轮换），托盘菜单的“打开日志目录”和设置中心的“日志”页都可以查看。控制台不可见时，致命配置/启动错误会额外弹一个 MessageBox，避免程序一闪而过没有任何提示。

实现与验收记录见 [docs/SETTINGS_WINDOW.md](docs/SETTINGS_WINDOW.md)。

**离线播放**：缓存目录的 `index.json` 记录 ratingKey、标题、thumb 和所属媒体库；断网时用缓存轮播，恢复后自动切回在线。旧版 `1479.jpg`、`标题_1479.jpg` 会自动扫描进索引；Plex 不可用且完全没有缓存时，常驻模式会保持后台等待。

**缓存文件名**：默认 `标题_ratingKey.jpg`（如 `阿凡达 水之道_1479.jpg`）；`--cache-naming key` 改为 `1479.jpg`。命中旧命名会直接改名，不重新下载。

**缓存维护**：`cache_days` 按天数清理，`cache_max_mb` 限制总容量（默认 2048，0 表示不限），运行期间每小时或每新下载 50 张检查一次；Plex 海报 thumb 版本变化时会重新下载，不会一直用旧图。

## 命令行

`--help` 查看完整参数。常用：

| 参数 | 说明 |
|---|---|
| `--config <文件>` | 指定配置文件 |
| `--library <编号/名称>` | 选择媒体库，可重复 |
| `--display <秒>` | 每张停留时间 |
| `--layout single\|wall` | 单张海报 / 6 张 3×2 海报墙 |
| `--transition fade\|slide-left\|slide-right\|slide-up\|slide-down\|random` | 切换效果；方向表示从哪一侧滑入 |
| `--fade <秒>` | 切换时长，0 表示关闭（旧参数名，也控制滑入时长） |
| `--motion animated\|static` | 动效模式；`static` 完全静止但仍轮播 |
| `--no-motion` | 等同 `--motion static`（保留兼容） |
| `--cache-dir <目录>` | 缓存位置，`auto` 自动挑选 |
| `--cache-days <天>` | 缓存保留天数 |
| `--cache-max-mb <MB>` | 缓存容量上限，0 表示不限 |
| `--watch` / `--now` | 常驻等待 / 立刻全屏 |
| `--background` | 后台运行，不打开设置页（开机自启/重启使用） |
| `--preview` | 1280x720 窗口预览 |
| `--self-test <N>` / `--snapshot <png>` | 无界面自检 / 导出结果（按当前布局） |

命令行优先于配置文件。例如：

```powershell
# 海报墙 + 从左侧滑入，每 8 秒换一组
.\target\release\plex-poster-screensaver.exe --config config.json --preview --layout wall --transition slide-left --display 8

# 单张完全静止，只按间隔换图
.\target\release\plex-poster-screensaver.exe --config config.json --preview --motion static

# 海报墙 + 静止模式（优先级最高，无转场、无缩放）
.\target\release\plex-poster-screensaver.exe --config config.json --preview --layout wall --transition random --motion static
```

普通全屏模式下按 ESC、点击或移动鼠标退出；`--preview` 不会因鼠标移动退出；`--watch` 有输入时收起并继续等待下一轮。

## 许可

[MIT](LICENSE) © 2026 breeze

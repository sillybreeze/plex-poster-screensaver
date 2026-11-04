# plex-poster-screensaver

Plex 海报屏保：从媒体库随机取电影/剧集海报全屏轮播，自动提取主色做氛围光，并带 Ken Burns 轻动效。Rust + D3D11 实现。

> Windows screensaver that displays your Plex movie and TV posters full-screen, with ambient color glow and Ken Burns motion.

[![Release](https://img.shields.io/github/v/release/sillybreeze/plex-poster-screensaver?label=release)](https://github.com/sillybreeze/plex-poster-screensaver/releases/latest)

![截图](docs/screenshot.png)

## 特性

- **氛围光 + Ken Burns**：按主色生成渐变、模糊纹理和暗角；10 秒缓慢推近 3.5%
- **D3D11 渲染**：缩放和双线性采样交给 GPU，每帧 CPU 约 0.05 ms
- **后台预取**：下载、解码、合成、取色都在工作线程，主界面不卡
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

```powershell
$startup = [Environment]::GetFolderPath('Startup')
$ws = New-Object -ComObject WScript.Shell
$lnk = $ws.CreateShortcut("$startup\Plex 海报屏保.lnk")
$lnk.TargetPath = "D:\path\to\plex-poster-screensaver.exe"
$lnk.Arguments = '--config "D:\path\to\config.json" --watch'
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
  "fade_seconds": 1.2,
  "cache_dir": "covers",
  "cache_naming": "title",
  "cache_days": 30,
  "cache_max_mb": 2048,
  "ken_burns": true,
  "motion_fps": 30
}
```

`PLEX_URL` / `PLEX_TOKEN` 可替代配置里的地址和 Token。`cache_dir` 的相对路径按 **config.json 所在目录** 解析；命令行 `--cache-dir` 的相对路径按当前工作目录。显式 `--config` 读取或解析失败会直接报错。

**离线播放**：缓存目录的 `index.json` 记录 ratingKey、标题、thumb 和所属媒体库；断网时用缓存轮播，恢复后自动切回在线。旧版 `1479.jpg`、`标题_1479.jpg` 会自动扫描进索引；Plex 不可用且完全没有缓存时，常驻模式会保持后台等待。

**缓存文件名**：默认 `标题_ratingKey.jpg`（如 `阿凡达 水之道_1479.jpg`）；`--cache-naming key` 改为 `1479.jpg`。命中旧命名会直接改名，不重新下载。

**缓存维护**：`cache_days` 按天数清理，`cache_max_mb` 限制总容量（默认 2048，0 表示不限），运行期间每小时或每新下载 50 张检查一次；Plex 海报 thumb 版本变化时会重新下载，不会一直用旧图。

## 命令行

`--help` 查看完整参数。常用：

| 参数 | 说明 |
|---|---|
| `--config <文件>` | 指定配置文件 |
| `--library <编号/名称>` | 选择媒体库，可重复 |
| `--display <秒>` / `--fade <秒>` | 停留时间 / 淡入淡出 |
| `--no-motion` | 关闭 Ken Burns 动效（轮播继续） |
| `--cache-dir <目录>` | 缓存位置，`auto` 自动挑选 |
| `--cache-days <天>` | 缓存保留天数 |
| `--cache-max-mb <MB>` | 缓存容量上限，0 表示不限 |
| `--watch` / `--now` | 常驻等待 / 立刻全屏 |
| `--preview` | 1280x720 窗口预览 |
| `--self-test <N>` / `--snapshot <png>` | 无界面自检 / 导出结果 |

普通全屏模式下按 ESC、点击或移动鼠标退出；`--preview` 不会因鼠标移动退出；`--watch` 有输入时收起并继续等待下一轮。

## 许可

[MIT](LICENSE) © 2026 breeze

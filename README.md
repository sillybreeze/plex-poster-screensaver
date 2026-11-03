# plex-poster-screensaver

Plex 海报屏保 —— 从 Plex 媒体库随机取电影/剧集海报全屏轮播，自动提取画面主色做氛围光，
并带缓慢推近的 Ken Burns 动效。Rust + D3D11 实现。

> Windows screensaver that displays your Plex movie and TV posters full-screen, with ambient color glow and Ken Burns motion.

![截图](docs/screenshot.png)

## 特性

- **取色氛围光**：按「占比 × 鲜艳度 × 亮度」从海报里挑主色，生成竖向渐变 + 模糊纹理 + 暗角，黑底海报也不会变成一片黑
- **Ken Burns 轻动效**：10 秒内推近 3.5% 并轻微漂移，方向逐张轮换；海报铺满屏幕高度，上下不留黑边
- **GPU 渲染**：D3D11 把缩放和双线性采样交给显卡，每帧 CPU 约 0.05 ms，帧率与 CPU 占用基本无关
- **亚像素平滑**：硬件采样天然连续，不会出现「几帧不动、突然跳 1 像素」的卡顿
- **后台预取**：下载、解码、合成、取色都在工作线程，主界面不卡
- **本地缓存**：按「标题_编号」命名，一眼认得出片子又不会重名；兼容旧版的标题名和纯编号命名，过期自动清理
- **常驻等待**：`--watch` 启动后不占屏，空闲到点自动全屏，动一下鼠标/键盘立刻收起，循环往复

## 快速开始

需要 Rust 1.75+ 和 Windows 10/11。

```bash
cargo build --release
# 产物：target/release/plex-poster-screensaver.exe
```

```powershell
# 首次运行会提示选择媒体库（回车 = 全部）
.\target\release\plex-poster-screensaver.exe

# 列出媒体库编号
.\target\release\plex-poster-screensaver.exe --list

# 窗口预览，方便调试（鼠标移动不会退出）
.\target\release\plex-poster-screensaver.exe --preview --display 3

# 无界面自检：取 5 张并打印耗时
.\target\release\plex-poster-screensaver.exe --self-test 5

# 导出当前合成结果
.\target\release\plex-poster-screensaver.exe --snapshot out.png
```

## 常驻等待模式（不写注册表）

不想走 Windows 那套 `.scr` 屏保（要往注册表写 `SCRNSAVE.EXE`）就用这个：
程序跑起来后只是藏着等，**空闲到点自动全屏**，动鼠标或按键立刻收起，然后继续等。

```powershell
.\target\release\plex-poster-screensaver.exe --config config.json --watch
```

| 参数 | 说明 |
|---|---|
| `--watch` | 开启常驻等待 |
| `--idle <分钟>` | 空闲多久触发，默认 5；可写小数（`0.1` = 6 秒），最小 5 秒 |
| `--mouse-only` | 只把鼠标当活动，键盘不算 |
| `--hotkey <组合键>` | 立刻开始的全局热键，默认 `Ctrl+Alt+P` |
| `--no-hotkey` | 停用全局热键 |
| `--now` | 不等空闲，立刻全屏（覆盖配置里的 `watch`） |

也可以固化在 `config.json` 里，启动时不用带参数：

```json
"watch": true,
"idle_minutes": 5,
"mouse_only": false,
"hotkey": "Ctrl+Alt+P"
```

### 想立刻开始

三种方式，都不用改配置。

**按热键。** 常驻时会注册一个全局热键，默认 `Ctrl+Alt+P`，任何程序在最前面时按一下都能立刻全屏：

```powershell
--hotkey "Ctrl+Shift+F9"   # 换个不冲突的组合
--no-hotkey                # 不要热键
```

支持 `Ctrl` / `Alt` / `Shift` / `Win` 加单个按键：字母、数字、`F1`~`F24`、`Space`、`Tab`、
`Enter`、`Esc`、方向键等。被别的程序先占了会打印一行提示，其它触发方式照常工作。

**再启动一次。** 常驻模式是单实例的：已经有一个在跑时，新启动的那个只会把「立刻开始」的信号
递过去然后自己退出，不会开第二个窗口。所以双击同一个快捷方式就等于「现在就开始」。

```powershell
# 平时挂着
.\target\release\plex-poster-screensaver.exe --config config.json --watch

# 想看了，再跑一次同样的命令（或双击快捷方式）
```

**`--now`。** 完全不走等待，直接全屏，适合单独做个「立刻播放」的快捷方式：

```powershell
.\target\release\plex-poster-screensaver.exe --config config.json --now
```

两者都受「动鼠标/按键就收起」约束，收起后 `--watch` 那个继续等下一轮，`--now` 那个直接退出。

### 判定口径

默认走 Windows 的 `GetLastInputInfo`，**键盘和鼠标都算活动**，和系统屏保、自动锁屏是同一个标准。

`--mouse-only` 换成轮询鼠标位置和左/右/中键，键盘完全不计入。代价是：你连着打字五分钟，
屏保也会弹出来（弹出来会被下一次按键立刻收掉）。

不管哪种模式，**收起屏保的条件都是"有任何输入"**——鼠标动、点击、按键都算，不然全屏之后就退不出来了。

### 空闲时不干活

没触发的时候窗口是隐藏的、不渲染、不 `Present`，每 500ms 只读一次时间戳；
D3D11 设备和纹理也要等第一次触发才创建。所以常驻一整天基本是零 CPU。

### 开机自启（用启动文件夹，不碰注册表）

```powershell
$startup = [Environment]::GetFolderPath('Startup')
$ws = New-Object -ComObject WScript.Shell
$lnk = $ws.CreateShortcut("$startup\Plex 海报屏保.lnk")
$lnk.TargetPath = "D:\PROJECT\PJ2\target\release\plex-poster-screensaver.exe"
$lnk.Arguments = '--config "D:\PROJECT\PJ2\config.json" --watch'
$lnk.Save()
```

`--config` 别省：exe 在 `target\release` 里，默认只会找**exe 同目录**和 `%LOCALAPPDATA%` 下的
`config.json`，项目根目录那份它是看不到的。

## 配置

优先级：命令行 > 环境变量 > `config.json` > 默认值。

```bash
cp config.example.json config.json
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
  "ken_burns": true,
  "motion_fps": 30
}
```

也可以用环境变量 `PLEX_URL` / `PLEX_TOKEN`，避免把 Token 写进文件。

`config.json` 放在 exe 同目录会被自动读取；也可以用 `--config` 指定路径。

### 缓存文件名

默认 `阿凡达 水之道_1479.jpg`：前面是片名（去掉 `\ / : * ? " < > |` 这些 Windows 不允许的字符，
连续空格压成一个，最长 60 字），后面的 `1479` 是 Plex 的 `ratingKey`，保证同名不同版本不会互相覆盖。

`--cache-naming key` 换回纯编号的 `1479.jpg`（最短，但认不出是哪部）。

换格式**不会重新下载**：命中旧命名时直接 `rename` 成当前格式。认识三种名字：

| 名字 | 来源 |
|---|---|
| `阿凡达 水之道_1479.jpg` | 现在（`title`，默认） |
| `1479.jpg` | 1.0 的纯 ratingKey（`key`） |
| `阿凡达 水之道.jpg` | Python v3/v4 的纯标题 |

`prune` 只删自己命名的前两种；Python 版留下的纯标题文件永远不动，要清得自己动手删。

## 命令行

```
--config <文件>        指定配置文件
--list                 列出媒体库后退出
--library <编号或名称> 选择媒体库，可重复
--display <秒>         每张停留时间
--fade <秒>            淡入淡出时长，0 表示关闭
--motion-fps <帧率>    Ken Burns 帧率
--no-motion            关闭动效
--cache-dir <目录>     封面缓存位置（auto = 自动挑选）
--cache-naming <方式>  缓存文件名（title = 标题_编号，key = 纯编号）
--cache-days <天>      缓存保留天数
--no-transcode         不使用服务端缩图
--insecure             HTTPS 跳过证书校验
--watch                常驻等待，空闲到点自动全屏
--hotkey <组合键>      立刻开始的全局热键，默认 Ctrl+Alt+P
--no-hotkey            停用全局热键
--now                  立刻全屏，不等空闲
--idle <分钟>          空闲多久触发，默认 5
--mouse-only           只把鼠标算作活动
--preview              1280x720 窗口预览
--self-test <N>        无界面自检
--snapshot <png>       导出合成结果
```

运行时按 ESC、点击鼠标或移动鼠标即可退出。

## 项目结构

```
src/
├── main.rs     入口、CLI、list/self-test/snapshot
├── config.rs   参数与 config.json 解析
├── plex.rs     Plex 客户端 + 封面缓存
├── render.rs   图像管线：缩放、氛围光、Ken Burns 采样
├── gpu.rs      D3D11：纹理上传、着色器、交换链
├── idle.rs     空闲检测：GetLastInputInfo / 纯鼠标轮询
├── trigger.rs  单实例信令：再启动一次就通知已经跑着的实例
└── app.rs      winit 窗口、事件循环、动效调度、常驻等待
```

## 许可

[MIT](LICENSE) © 2026 breeze

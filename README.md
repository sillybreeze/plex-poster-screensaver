# plex-poster-screensaver

Windows 10/11 Plex 海报屏保。支持单张海报或海报墙、淡化/方向转场、静止轮播、离线缓存和托盘设置中心。

[![Release](https://img.shields.io/github/v/release/sillybreeze/plex-poster-screensaver?label=release)](https://github.com/sillybreeze/plex-poster-screensaver/releases/latest)

![截图](docs/screenshot.png)

## 使用

从 [Releases](https://github.com/sillybreeze/plex-poster-screensaver/releases/latest) 下载 Windows x64 版本。首次双击打开设置中心，填写 Plex 地址和 Token，测试连接并选择媒体库，保存后程序会启动。

屏保运行后可从系统托盘打开设置、立即播放、暂停自动触发、查看日志或退出。设置保存后会重启运行实例以应用更改。

开机自启可在设置中心“启动与热键”页启用。程序会在当前用户的 Windows“启动”文件夹添加隐藏运行脚本；如果系统拒绝访问该目录，请检查文件权限，或手动处理设置页提示的脚本路径。

## 默认播放

- 单张海报居中显示，背景使用海报主色生成氛围效果。
- 每张停留 10 秒，使用 0.6 秒淡化转场。
- 开启动效和 Ken Burns 缓慢推近，帧率为 30 FPS。
- 无命令行参数时双击打开设置；后台运行需使用 `--background`。

设置中心可更改布局、转场方向与时长、停留时间、动效模式、Plex 连接、缓存、常驻触发和热键。静止模式仍会按停留时间轮播，只是不执行转场和缩放。配置文件中的常用显示字段如下：

```json
{
  "display_layout": "single",
  "transition": "fade",
  "transition_seconds": 0.6,
  "display_seconds": 10,
  "motion_mode": "animated",
  "ken_burns": true
}
```

布局可选 `single`、`wall`；转场可选 `fade`、`slide_left`、`slide_right`、`slide_up`、`slide_down`、`random`；动效可选 `animated`、`static`。海报墙最多显示 6 张，短列表会自动调整网格。

## 命令行

命令行选项优先于配置文件。设置中心适合日常使用，以下命令适合预览、自动化和排查：

```powershell
# 窗口预览：海报墙、从左滑入，每 8 秒换一组
plex-poster-screensaver.exe --config config.json --preview --layout wall --transition slide-left --display 8

# 静止轮播
plex-poster-screensaver.exe --config config.json --preview --motion static

# 常驻等待；默认空闲 5 分钟后全屏
plex-poster-screensaver.exe --config config.json --watch

# 列出媒体库 / 自检 / 导出快照
plex-poster-screensaver.exe --config config.json --list
plex-poster-screensaver.exe --config config.json --self-test 5
plex-poster-screensaver.exe --config config.json --snapshot out.png
```

`--help` 查看全部参数。常用选项：`--idle <分钟>`、`--mouse-only`、`--hotkey <组合键>`、`--no-hotkey`、`--now`、`--fade <秒>`、`--cache-dir <目录>`。

## 配置与日志

首次配置由设置中心创建。也可以复制 `config.example.json` 为 `config.json`，填写 `plex_url` 和 `plex_token`。配置优先级为命令行 > 环境变量 > 配置文件 > 默认值；`PLEX_URL`、`PLEX_TOKEN` 可通过环境变量提供。

缓存目录内的索引会记录媒体库和海报信息。断网时程序会尝试用本地缓存继续播放，并在后台重连。日志位于 `%LOCALAPPDATA%\PlexPosterScreensaver\logs\app.log`，也可从托盘菜单或设置中心打开。

配置参考：[config.example.json](config.example.json)。设置中心实现与验收记录：[docs/SETTINGS_WINDOW.md](docs/SETTINGS_WINDOW.md)。历史开发计划与验收记录见 [docs](docs)。

## 从源码构建

需要 Windows 10/11、Rust 稳定版和 MSVC C++ Build Tools：

```powershell
cargo build --release --offline
cargo test --release --offline
cargo clippy --release --offline --all-targets
```

## 许可

[MIT](LICENSE) © 2026 breeze

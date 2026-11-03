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
- **本地缓存**：按 `ratingKey` 命名，兼容旧版按标题命名的文件，过期自动清理

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
  "cache_days": 30,
  "ken_burns": true,
  "motion_fps": 30
}
```

也可以用环境变量 `PLEX_URL` / `PLEX_TOKEN`，避免把 Token 写进文件。

`config.json` 放在 exe 同目录会被自动读取；也可以用 `--config` 指定路径。

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
--cache-days <天>      缓存保留天数
--no-transcode         不使用服务端缩图
--insecure             HTTPS 跳过证书校验
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
└── app.rs      winit 窗口、事件循环、动效调度
```

## 许可

[MIT](LICENSE) © 2026 breeze

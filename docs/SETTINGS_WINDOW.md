# 设置中心

## 目标

把日常需要的持久配置从 `config.json` 搬进 GUI，同时保留配置文件作为自动化、便携部署和高级字段的入口。设置窗口不再是只改六项显示参数的窄对话框，而是按用途分页的设置中心。

## 页面

| 页面 | 内容 | 写回字段 |
| --- | --- | --- |
| 显示 | 布局、切换效果、切换时长、停留时间、动效模式、Ken Burns、预览 | `display_layout`、`transition`、`transition_seconds`、`display_seconds`、`motion_mode`、`ken_burns` |
| Plex 服务器 | 地址、Token、验证 HTTPS、超时、测试连接、媒体库多选 | `plex_url`、`plex_token`、`verify_tls`、`timeout`、`libraries` |
| 启动与热键 | 常驻等待、空闲分钟数、只算鼠标、全局热键、开机自启 | `watch`、`idle_minutes`、`mouse_only`、`hotkey`（自启是 OS 状态） |
| 缓存 | 目录/自动选择、命名、保留天数、容量上限、transcode、统计/清理 | `cache_dir`、`cache_naming`、`cache_days`、`cache_max_mb`、`transcode` |
| 高级 | 动效帧率、配置文件路径、打开配置文件/日志目录 | `motion_fps` |
| 日志 | `app.log` 最近 300 行，只读、可滚动、每秒刷新 | 无 |

窗口按系统 DPI 缩放；左侧分页，底部固定“取消/保存”，显示页保留“恢复显示默认”。

## Plex 配置与首次启动

- **Token 掩码**：默认以密码框显示，可勾选“显示 Token”；测试失败不会回显 Token。
- **测试连接**：在后台线程构造临时 `Config` 并调用 `Client::sections()`，通过 `PostMessage` 回到设置窗口；成功后在列表里显示实际媒体库，按保存的标题恢复勾选。
- **媒体库**：多选列表；全选时保存 `libraries: []`（等价于全部），部分选择时保存标题。修改媒体库名称后需要重新测试/选择。
- **首次启动**：现在双击 exe（无参数）只打开设置页；Token 为空时设置页直接落在 Plex 页。保存后如果没有正在运行的后台实例，程序会按新配置启动一个 `--background` 实例；如果已有实例，则请求它重启。`--background` 是后台运行的显式入口，开机自启脚本也使用它。
- **覆盖提示**：窗口顶部显示当前被命令行/环境变量覆盖的字段；保存只改文件，不改变本次运行的优先级链（默认值 → 文件 → `PLEX_URL`/`PLEX_TOKEN` → CLI）。

## 配置模型

- `EditableSettings` 保存完整持久配置，和运行期 `Config` 分开；`cache_dir` 保留 `auto` 语义，相对路径按配置文件目录解析。
- `load_editable_settings` / `save_editable_settings` 读写 JSON；保存只覆盖已知字段，未知/高级字段原样保留，旧 `fade_seconds` 迁移为 `transition_seconds`。
- `save_preserving` 统一“读现有 JSON → 应用变更 → 临时文件 → rename”的原子写入路径。
- `SettingsOverrides::from_args` 汇总 CLI/env 覆盖项，用于窗口顶部提示。
- 保存前统一校验 URL、Token（有环境变量时允许空）、超时、空闲时间、帧率、缓存天数/目录和热键格式。

## 保存与立即生效

- 保存先写 `config.json`，再同步开机自启状态，最后请求重启：
  - 设置窗口在运行中的后台实例里打开：通过事件循环重启，主进程退出前释放单实例锁，然后以原参数启动新进程。
  - 设置窗口是双击 exe 启动的独立进程：通过托盘窗口消息 `WM_RESTART` 请求已有后台实例重启；没有实例时，由设置进程启动 `--background`。
- 托盘菜单新增 **重启程序**，走同一条 `RunOutcome::Restart` 路径。
- 保存后不再提示“下次启动才生效”；窗口直接关闭并重启/启动。
- 配置文件不存在时自动新建；没有可写位置或现有 JSON 损坏时禁用保存，不覆盖损坏文件。
- 缓存目录改变通过重启生效，旧目录不会自动搬移；缓存页会明确提示。

## 开机自启

- 勾选“开机自启”后在当前用户启动文件夹写入 `PlexPosterScreensaver.vbs`，内容为隐藏运行当前 exe；登录后按“常驻等待”设置运行：

  ```vbscript
  Set shell = CreateObject("WScript.Shell")
  shell.Run """<exe>"" --background --config ""<config>""", 0, False
  ```

- 不写注册表、不申请管理员权限；取消勾选删除该脚本。
- 文件用 UTF-16 LE + BOM 写入，兼容中文用户名/路径。
- 受企业策略禁用 Windows Script Host 的机器上，开机自启不会生效；设置页会显示写入错误。

## 缓存统计与清理

- 缓存页显示当前运行目录的 `.jpg` 数量和总大小（内存模式显示“未落盘”）。
- **刷新统计**重新扫描；**清理缓存**二次确认后删除 `.jpg` 和未完成 `.part` 文件，并清空 `index.json` 的条目（保留媒体库列表）。
- 清理只影响当前缓存目录；清理后按需重新下载，正在播放的已解码帧不受影响。

## 控制台与日志

- 后台启动（`--background`/开机自启/计划任务）如果控制台是进程独占的，`FreeConsole` 释放；从 cmd/PowerShell 启动时保留共享控制台，CLI 输出仍可用。双击无参数启动时释放控制台并直接显示设置页。
- 日志固定写入 `%LOCALAPPDATA%\PlexPosterScreensaver\logs\app.log`，5MB 轮换；日志页只读显示当前文件最近 300 行，按文件长度变化刷新。
- 控制台不可见时，启动致命错误额外弹 MessageBox。

## 实现位置

- `src/config.rs`：`EditableSettings`、`SettingsOverrides`、读写/校验/迁移。
- `src/settings.rs`：分页窗口、原生控件、后台连接测试、首次配置模态循环、保存/重启请求、缓存统计清理。
- `src/settings_preview.rs`：CPU 预览合成，移到独立模块。
- `src/autostart.rs`：启动文件夹 VBS 的读写。
- `src/platform.rs`：托盘菜单（含“重启程序”）、跨进程重启消息、设置路径/缓存/覆盖信息传递。
- `src/app.rs` / `src/main.rs`：覆盖信息传递、`RunOutcome` 重启、无参数设置入口。

## 验证

- `cargo test --release --offline`：108 个测试通过；`cargo clippy --release --offline --all-targets` 无警告。
- GUI 验证：
  - 六个页面逐页截图，Plex 测试连接成功并列出 4 个媒体库；
  - 保存 `wall / slide-left / 0.8s / 8s / static / Ken Burns off`、`idle=7`、`mouse_only=true`、`cache_days=45`、`cache_max_mb=1024`、`motion_fps=24` 全部写回；
  - 高级字段 `custom_advanced` 保留；
  - 无参数启动直接进入设置页；保存后自动启动后台实例；
  - 从托盘打开设置保存后，后台进程 PID 变化（立即重启）；
  - 托盘“重启程序”后 PID 变化；
  - 清理缓存按钮删除测试文件，统计从 `1 张 / 1KB` 变为 `0 张`；
  - 开机自启脚本能写入启动文件夹，取消后删除。
- 之前验证的独立控制台 `控制台状态: Hidden` 和 `--help` 输出仍成立。

## 限制

- 保存会重启后台实例；全屏播放中保存会有一次短暂重建。
- 媒体库选择保存标题而不是 Plex section key；重命名后需要重新选择。
- 缓存目录变更通过重启生效，不搬移旧缓存；“清理缓存”只清当前目录的封面和索引条目。
- 开机自启依赖 Windows Script Host；被策略禁用时只能手动使用 `--background` 启动。
- Token 仍未 DPAPI 加密；暂无多配置文件切换。
- 设置窗口的预览是加速近似，缓存不足时用占位色块。

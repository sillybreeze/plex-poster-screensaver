# 设置中心

## 目标

把日常需要的持久配置从 `config.json` 搬进 GUI，同时保留配置文件作为自动化、便携部署和高级字段的入口。设置窗口不再是只改六项显示参数的窄对话框，而是按用途分页的设置中心。

## 页面

| 页面 | 内容 | 写回字段 |
| --- | --- | --- |
| 显示 | 布局、切换效果、切换时长、停留时间、动效模式、Ken Burns、预览 | `display_layout`、`transition`、`transition_seconds`、`display_seconds`、`motion_mode`、`ken_burns` |
| Plex 服务器 | 地址、Token、验证 HTTPS、超时、测试连接、媒体库多选 | `plex_url`、`plex_token`、`verify_tls`、`timeout`、`libraries` |
| 启动与热键 | 常驻等待、空闲分钟数、只算鼠标、全局热键 | `watch`、`idle_minutes`、`mouse_only`、`hotkey` |
| 缓存 | 目录/自动选择、命名、保留天数、容量上限、transcode | `cache_dir`、`cache_naming`、`cache_days`、`cache_max_mb`、`transcode` |
| 高级 | 动效帧率、配置文件路径、打开配置文件/日志目录 | `motion_fps` |
| 日志 | `app.log` 最近 300 行，只读、可滚动、每秒刷新 | 无 |

窗口按系统 DPI 缩放；左侧分页，底部固定“取消/保存”，显示页保留“恢复显示默认”。

## Plex 配置与首次启动

- **Token 掩码**：默认以密码框显示，可勾选“显示 Token”；测试失败不会回显 Token。
- **测试连接**：在后台线程构造临时 `Config` 并调用 `Client::sections()`，通过 `PostMessage` 回到设置窗口；成功后在列表里显示实际媒体库，按保存的标题恢复勾选。
- **媒体库**：多选列表；全选时保存 `libraries: []`（等价于全部），部分选择时保存标题。修改媒体库名称后需要重新测试/选择。
- **首次启动**：`config::load` 后如果 Token 为空且不是工具命令，先以模态方式打开 Plex 配置页；用户保存后重新加载配置，Token 仍为空才报错退出。没有配置文件时，保存目标由 `display_settings_path` 在 exe 目录或 `%LOCALAPPDATA%\PlexPosterScreensaver\` 中选择。
- **覆盖提示**：窗口顶部显示当前被命令行/环境变量覆盖的字段；保存只改文件，不改变本次运行的优先级链（默认值 → 文件 → `PLEX_URL`/`PLEX_TOKEN` → CLI）。

## 配置模型

- `EditableSettings` 保存完整持久配置，和运行期 `Config` 分开；`cache_dir` 保留 `auto` 语义，相对路径按配置文件目录解析。
- `load_editable_settings` / `save_editable_settings` 读写 JSON；保存只覆盖已知字段，未知/高级字段原样保留，旧 `fade_seconds` 迁移为 `transition_seconds`。
- `save_preserving` 统一“读现有 JSON → 应用变更 → 临时文件 → rename”的原子写入路径。
- `SettingsOverrides::from_args` 汇总 CLI/env 覆盖项，用于窗口顶部提示。
- 保存前统一校验 URL、Token（有环境变量时允许空）、超时、空闲时间、帧率、缓存天数/目录和热键格式。

## 保存与生效

- 保存只写 `config.json`；显示、Plex、缓存、触发等变更**下次启动生效**。窗口提示这一点，不假装热更新。
- 配置文件不存在时自动新建；没有可写位置或现有 JSON 损坏时禁用保存，不覆盖损坏文件。
- 缓存目录改变只影响下次启动，旧目录不会自动搬移；缓存页会明确提示。

## 控制台与日志

- 正常启动（双击/开机自启/计划任务）如果控制台是进程独占的，`FreeConsole` 释放；从 cmd/PowerShell 启动时保留共享控制台，CLI 输出仍可用。
- 日志固定写入 `%LOCALAPPDATA%\PlexPosterScreensaver\logs\app.log`，5MB 轮换；日志页只读显示当前文件最近 300 行，按文件长度变化刷新。
- 控制台不可见时，启动致命错误额外弹 MessageBox。

## 实现位置

- `src/config.rs`：`EditableSettings`、`SettingsOverrides`、读写/校验/迁移。
- `src/settings.rs`：分页窗口、原生控件、后台连接测试、首次配置模态循环、保存。
- `src/settings_preview.rs`：CPU 预览合成，移到独立模块。
- `src/platform.rs`：托盘菜单、设置路径/缓存/覆盖信息传递。
- `src/app.rs` / `src/main.rs`：覆盖信息传递、首次启动流程。

## 验证

- `cargo test --release --offline`：105 个测试通过；`cargo clippy --release --offline --all-targets` 无警告。
- GUI 验证：
  - 六个页面逐页截图，Plex 测试连接成功并列出 4 个媒体库；
  - 保存 `wall / slide-left / 0.8s / 8s / static / Ken Burns off`、`idle=7`、`mouse_only=true`、`cache_days=45`、`cache_max_mb=1024`、`motion_fps=24` 全部写回；
  - 高级字段 `custom_advanced` 保留；
  - 首次启动（Token 为空）自动打开 Plex 页，填入 Token、测试连接、保存后应用继续启动；
  - 日志页能显示 `app.log` 内容。
- 之前验证的独立控制台 `控制台状态: Hidden` 和 `--help` 输出仍成立。

## 限制

- 保存后不热应用，仍需重启屏保进程。
- 媒体库选择保存标题而不是 Plex section key；重命名后需要重新选择。
- 缓存目录变更不搬移旧缓存；暂无缓存清理/统计按钮。
- 暂无 Windows 开机自启开关、Token DPAPI 加密、多配置文件切换。
- 设置窗口的预览是加速近似，缓存不足时用占位色块。

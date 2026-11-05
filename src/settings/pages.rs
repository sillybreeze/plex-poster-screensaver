//! 设置中心六个页面的原生控件布局。
//!
//! 页面布局和状态/保存/消息循环分开，便于单独调整页面而不碰数据读写逻辑。

use super::*;

pub(super) struct BuiltPages {
    pub(super) pages: [Vec<HWND>; 6],
    pub(super) preview: HWND,
    pub(super) log_view: HWND,
    pub(super) preview_size: (i32, i32),
}

type DisplayPage = (Vec<HWND>, HWND, (i32, i32));

pub(super) fn build(
    hwnd: HWND,
    instance: HINSTANCE,
    s: &impl Fn(i32) -> i32,
    font: HFONT,
    overrides: &SettingsOverrides,
) -> Result<BuiltPages, String> {
    let mut pages: [Vec<HWND>; 6] = std::array::from_fn(|_| Vec::new());
    let content_x = s(152);
    let content_w = s(732);

    let override_label = create_static_id(
        hwnd,
        "",
        content_x,
        s(16),
        content_w,
        s(36),
        ID_OVERRIDE,
        instance,
        font,
    )?;
    let override_text = overrides.summary();
    if override_text.is_empty() {
        unsafe {
            let _ = ShowWindow(override_label, SW_HIDE);
        }
    } else {
        set_window_text(
            hwnd,
            ID_OVERRIDE,
            &format!(
                "当前运行受启动参数/环境变量覆盖：{override_text}。这里的保存不会改变本次运行。"
            ),
        );
    }

    for page in ALL_PAGES {
        let y = s(64) + page.index() as i32 * s(40);
        create_button(
            hwnd,
            page.label(),
            s(16),
            y,
            s(120),
            s(34),
            page.nav_id(),
            instance,
            font,
        )?;
    }

    let (display, preview, preview_size) = display_page(hwnd, instance, s, font)?;
    pages[Page::Display.index()] = display;

    pages[Page::Plex.index()] = plex_page(hwnd, instance, s, font)?;
    pages[Page::Startup.index()] = startup_page(hwnd, instance, s, font)?;
    pages[Page::Cache.index()] = cache_page(hwnd, instance, s, font)?;
    pages[Page::Advanced.index()] = advanced_page(hwnd, instance, s, font)?;

    let log_view = logs_page(hwnd, instance, s, font)?;
    pages[Page::Logs.index()] = vec![log_view];

    Ok(BuiltPages {
        pages,
        preview,
        log_view,
        preview_size,
    })
}

#[allow(clippy::vec_init_then_push)]
fn display_page(
    hwnd: HWND,
    instance: HINSTANCE,
    s: &impl Fn(i32) -> i32,
    font: HFONT,
) -> Result<DisplayPage, String> {
    let form_x = s(164);
    let label_w = s(112);
    let control_x = s(284);
    let control_w = s(112);
    let rows = [s(64), s(104), s(144), s(184), s(224)];
    let mut display = Vec::new();
    for (index, label) in [
        "布局",
        "切换效果",
        "切换时长（秒）",
        "每张停留（秒）",
        "动效模式",
    ]
    .iter()
    .enumerate()
    {
        display.push(create_static(
            hwnd,
            label,
            form_x,
            rows[index],
            label_w,
            s(20),
            instance,
            font,
        )?);
    }
    let layout_combo = create_combo(
        hwnd,
        control_x,
        rows[0],
        control_w,
        s(180),
        ID_LAYOUT,
        false,
        instance,
        font,
    )?;
    add_combo_items(layout_combo, &["单张海报", "6 张海报墙（3×2）"]);
    display.push(layout_combo);

    let transition_combo = create_combo(
        hwnd,
        control_x,
        rows[1],
        control_w,
        s(180),
        ID_TRANSITION,
        false,
        instance,
        font,
    )?;
    add_combo_items(
        transition_combo,
        &[
            "淡化",
            "从左侧滑入",
            "从右侧滑入",
            "从上方滑入",
            "从下方滑入",
            "随机方向",
        ],
    );
    display.push(transition_combo);

    let transition_seconds = create_combo(
        hwnd,
        control_x,
        rows[2],
        control_w,
        s(180),
        ID_TRANSITION_SECONDS,
        true,
        instance,
        font,
    )?;
    add_combo_items(
        transition_seconds,
        &[
            "0.2", "0.3", "0.4", "0.6", "0.8", "1.0", "1.2", "1.5", "2.0", "2.5", "3.0",
        ],
    );
    display.push(transition_seconds);

    let display_seconds = create_combo(
        hwnd,
        control_x,
        rows[3],
        control_w,
        s(180),
        ID_DISPLAY_SECONDS,
        true,
        instance,
        font,
    )?;
    add_combo_items(display_seconds, &["3", "5", "8", "10", "15", "30", "60"]);
    display.push(display_seconds);

    let motion_combo = create_combo(
        hwnd,
        control_x,
        rows[4],
        control_w,
        s(180),
        ID_MOTION,
        false,
        instance,
        font,
    )?;
    add_combo_items(motion_combo, &["动画", "静止"]);
    display.push(motion_combo);

    display.push(create_checkbox(
        hwnd,
        "缓慢推近（Ken Burns）",
        form_x,
        s(264),
        s(250),
        s(24),
        ID_KEN_BURNS,
        instance,
        font,
    )?);

    let preview_x = s(420);
    let preview_y = s(64);
    let preview_w = s(448);
    let preview_h = ((preview_w as f32) * 9.0 / 16.0).round() as i32;
    let preview = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            w!("PlexPosterScreensaverSettingsPreview"),
            w!(""),
            WS_CHILD | WS_VISIBLE,
            preview_x,
            preview_y,
            preview_w,
            preview_h,
            hwnd,
            HMENU(ID_PREVIEW as usize as *mut c_void),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建预览窗口失败: {err}"))?;
    set_font(preview, font);
    display.push(preview);

    let hint = create_static_id(
        hwnd,
        "预览为加速循环。保存后会重启后台实例以立即生效；命令行参数仍会覆盖对应项。",
        preview_x,
        preview_y + preview_h + s(8),
        preview_w,
        s(38),
        ID_HINT,
        instance,
        font,
    )?;
    display.push(hint);
    display.push(create_button(
        hwnd,
        "恢复显示默认",
        form_x,
        s(514),
        s(120),
        s(30),
        ID_DEFAULTS,
        instance,
        font,
    )?);

    Ok((display, preview, (preview_w, preview_h)))
}

#[allow(clippy::vec_init_then_push)]
fn plex_page(
    hwnd: HWND,
    instance: HINSTANCE,
    s: &impl Fn(i32) -> i32,
    font: HFONT,
) -> Result<Vec<HWND>, String> {
    let form_x = s(164);
    let mut plex = Vec::new();
    plex.push(create_static(
        hwnd,
        "服务器地址",
        form_x,
        s(64),
        s(100),
        s(20),
        instance,
        font,
    )?);
    let url = create_edit(
        hwnd,
        "",
        s(270),
        s(62),
        s(430),
        s(24),
        ID_PLEX_URL,
        false,
        false,
        false,
        instance,
        font,
    )?;
    plex.push(url);
    plex.push(create_static(
        hwnd,
        "Plex Token",
        form_x,
        s(104),
        s(100),
        s(20),
        instance,
        font,
    )?);
    let token = create_edit(
        hwnd,
        "",
        s(270),
        s(102),
        s(430),
        s(24),
        ID_PLEX_TOKEN,
        true,
        false,
        false,
        instance,
        font,
    )?;
    plex.push(token);
    plex.push(create_checkbox(
        hwnd,
        "显示 Token",
        s(706),
        s(102),
        s(150),
        s(24),
        ID_SHOW_TOKEN,
        instance,
        font,
    )?);
    plex.push(create_checkbox(
        hwnd,
        "验证 HTTPS 证书",
        s(270),
        s(140),
        s(200),
        s(24),
        ID_VERIFY_TLS,
        instance,
        font,
    )?);
    plex.push(create_static(
        hwnd,
        "超时（秒）",
        form_x,
        s(184),
        s(100),
        s(20),
        instance,
        font,
    )?);
    plex.push(create_edit(
        hwnd,
        "10",
        s(270),
        s(182),
        s(70),
        s(24),
        ID_TIMEOUT,
        false,
        false,
        false,
        instance,
        font,
    )?);
    plex.push(create_button(
        hwnd,
        "测试连接",
        s(620),
        s(181),
        s(100),
        s(28),
        ID_TEST,
        instance,
        font,
    )?);
    plex.push(create_static_id(
        hwnd,
        "填写地址和 Token 后测试连接。",
        s(270),
        s(222),
        s(590),
        s(40),
        ID_TEST_STATUS,
        instance,
        font,
    )?);
    plex.push(create_static(
        hwnd,
        "媒体库",
        form_x,
        s(274),
        s(100),
        s(20),
        instance,
        font,
    )?);
    plex.push(create_listbox(
        hwnd,
        s(270),
        s(270),
        s(560),
        s(150),
        ID_LIBRARIES,
        instance,
        font,
    )?);
    plex.push(create_static(
        hwnd,
        "不选任何一项表示使用全部媒体库；测试连接后可按实际媒体库勾选。",
        s(270),
        s(430),
        s(560),
        s(38),
        instance,
        font,
    )?);

    Ok(plex)
}

#[allow(clippy::vec_init_then_push)]
fn startup_page(
    hwnd: HWND,
    instance: HINSTANCE,
    s: &impl Fn(i32) -> i32,
    font: HFONT,
) -> Result<Vec<HWND>, String> {
    let form_x = s(164);
    let mut startup = Vec::new();
    startup.push(create_checkbox(
        hwnd,
        "常驻等待：空闲到点后自动全屏",
        form_x,
        s(64),
        s(360),
        s(24),
        ID_WATCH,
        instance,
        font,
    )?);
    startup.push(create_static(
        hwnd,
        "空闲（分钟）",
        form_x,
        s(104),
        s(100),
        s(20),
        instance,
        font,
    )?);
    startup.push(create_edit(
        hwnd,
        "5",
        s(270),
        s(102),
        s(90),
        s(24),
        ID_IDLE,
        false,
        false,
        false,
        instance,
        font,
    )?);
    startup.push(create_static(
        hwnd,
        "5 秒–1440 分钟",
        s(370),
        s(106),
        s(200),
        s(20),
        instance,
        font,
    )?);
    startup.push(create_checkbox(
        hwnd,
        "只把鼠标算作活动（键盘不算）",
        form_x,
        s(144),
        s(360),
        s(24),
        ID_MOUSE_ONLY,
        instance,
        font,
    )?);
    startup.push(create_static(
        hwnd,
        "立刻开始热键",
        form_x,
        s(184),
        s(100),
        s(20),
        instance,
        font,
    )?);
    startup.push(create_edit(
        hwnd,
        "",
        s(270),
        s(182),
        s(250),
        s(24),
        ID_HOTKEY,
        false,
        false,
        false,
        instance,
        font,
    )?);
    startup.push(create_static(
        hwnd,
        "留空或 none 表示停用；格式示例：Ctrl+Alt+P、Ctrl+Shift+F9。",
        s(270),
        s(224),
        s(560),
        s(40),
        instance,
        font,
    )?);
    startup.push(create_checkbox(
        hwnd,
        "开机自启（当前用户，后台运行）",
        form_x,
        s(272),
        s(420),
        s(24),
        ID_AUTOSTART,
        instance,
        font,
    )?);
    startup.push(create_static(
        hwnd,
        "登录后按“常驻等待”设置运行（未勾选则直接全屏）；不申请管理员权限。",
        s(270),
        s(304),
        s(560),
        s(40),
        instance,
        font,
    )?);

    Ok(startup)
}

#[allow(clippy::vec_init_then_push)]
fn cache_page(
    hwnd: HWND,
    instance: HINSTANCE,
    s: &impl Fn(i32) -> i32,
    font: HFONT,
) -> Result<Vec<HWND>, String> {
    let form_x = s(164);
    let mut cache_page = Vec::new();
    cache_page.push(create_static(
        hwnd,
        "缓存目录",
        form_x,
        s(64),
        s(100),
        s(20),
        instance,
        font,
    )?);
    cache_page.push(create_edit(
        hwnd,
        "",
        s(270),
        s(62),
        s(500),
        s(24),
        ID_CACHE_DIR,
        false,
        false,
        false,
        instance,
        font,
    )?);
    cache_page.push(create_button(
        hwnd,
        "浏览…",
        s(780),
        s(62),
        s(90),
        s(26),
        ID_BROWSE,
        instance,
        font,
    )?);
    cache_page.push(create_checkbox(
        hwnd,
        "自动选择可写目录（保存为 auto）",
        s(270),
        s(100),
        s(400),
        s(24),
        ID_CACHE_AUTO,
        instance,
        font,
    )?);
    cache_page.push(create_static(
        hwnd,
        "文件名",
        form_x,
        s(144),
        s(100),
        s(20),
        instance,
        font,
    )?);
    let naming = create_combo(
        hwnd,
        s(270),
        s(142),
        s(140),
        s(140),
        ID_CACHE_NAMING,
        false,
        instance,
        font,
    )?;
    add_combo_items(naming, &["标题_编号", "只留编号"]);
    cache_page.push(naming);
    cache_page.push(create_static(
        hwnd,
        "保留天数",
        form_x,
        s(184),
        s(100),
        s(20),
        instance,
        font,
    )?);
    cache_page.push(create_edit(
        hwnd,
        "30",
        s(270),
        s(182),
        s(100),
        s(24),
        ID_CACHE_DAYS,
        false,
        false,
        false,
        instance,
        font,
    )?);
    cache_page.push(create_static(
        hwnd,
        "0 表示不按天数清理",
        s(380),
        s(186),
        s(220),
        s(20),
        instance,
        font,
    )?);
    cache_page.push(create_static(
        hwnd,
        "容量上限 MB",
        form_x,
        s(224),
        s(100),
        s(20),
        instance,
        font,
    )?);
    cache_page.push(create_edit(
        hwnd,
        "2048",
        s(270),
        s(222),
        s(100),
        s(24),
        ID_CACHE_MAX_MB,
        false,
        false,
        false,
        instance,
        font,
    )?);
    cache_page.push(create_static(
        hwnd,
        "0 表示不限",
        s(380),
        s(226),
        s(180),
        s(20),
        instance,
        font,
    )?);
    cache_page.push(create_checkbox(
        hwnd,
        "优先使用服务端缩图（transcode）",
        s(270),
        s(264),
        s(360),
        s(24),
        ID_TRANSCODE,
        instance,
        font,
    )?);
    cache_page.push(create_static_id(
        hwnd,
        "",
        s(270),
        s(304),
        s(580),
        s(40),
        ID_CACHE_INFO,
        instance,
        font,
    )?);
    cache_page.push(create_static_id(
        hwnd,
        "",
        s(270),
        s(350),
        s(580),
        s(20),
        ID_CACHE_STATS,
        instance,
        font,
    )?);
    cache_page.push(create_button(
        hwnd,
        "刷新统计",
        s(270),
        s(382),
        s(120),
        s(28),
        ID_REFRESH_CACHE,
        instance,
        font,
    )?);
    cache_page.push(create_button(
        hwnd,
        "清理缓存",
        s(402),
        s(382),
        s(120),
        s(28),
        ID_CLEAN_CACHE,
        instance,
        font,
    )?);

    Ok(cache_page)
}

#[allow(clippy::vec_init_then_push)]
fn advanced_page(
    hwnd: HWND,
    instance: HINSTANCE,
    s: &impl Fn(i32) -> i32,
    font: HFONT,
) -> Result<Vec<HWND>, String> {
    let form_x = s(164);
    let mut advanced = Vec::new();
    advanced.push(create_static(
        hwnd,
        "动效帧率",
        form_x,
        s(64),
        s(100),
        s(20),
        instance,
        font,
    )?);
    advanced.push(create_edit(
        hwnd,
        "30",
        s(270),
        s(62),
        s(80),
        s(24),
        ID_MOTION_FPS,
        false,
        false,
        false,
        instance,
        font,
    )?);
    advanced.push(create_static(
        hwnd,
        "1–120；只影响动画模式",
        s(360),
        s(66),
        s(220),
        s(20),
        instance,
        font,
    )?);
    advanced.push(create_static(
        hwnd,
        "配置文件",
        form_x,
        s(104),
        s(100),
        s(20),
        instance,
        font,
    )?);
    advanced.push(create_static_id(
        hwnd,
        "",
        s(270),
        s(100),
        s(580),
        s(40),
        ID_CONFIG_PATH,
        instance,
        font,
    )?);
    advanced.push(create_button(
        hwnd,
        "打开配置文件",
        s(270),
        s(152),
        s(150),
        s(28),
        ID_OPEN_CONFIG,
        instance,
        font,
    )?);
    advanced.push(create_button(
        hwnd,
        "打开日志目录",
        s(430),
        s(152),
        s(150),
        s(28),
        ID_OPEN_LOGS,
        instance,
        font,
    )?);
    advanced.push(create_static(
        hwnd,
        "config.json 中未在界面出现的字段会原样保留；命令行和环境变量优先级更高。",
        s(270),
        s(200),
        s(580),
        s(40),
        instance,
        font,
    )?);

    Ok(advanced)
}

fn logs_page(
    hwnd: HWND,
    instance: HINSTANCE,
    s: &impl Fn(i32) -> i32,
    font: HFONT,
) -> Result<HWND, String> {
    let content_x = s(152);
    let content_w = s(732);
    create_log_view(hwnd, content_x, s(64), content_w, s(420), instance, font)
}

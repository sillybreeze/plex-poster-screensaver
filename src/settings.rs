//! 托盘菜单打开的设置中心。
//!
//! 左侧分页，覆盖 Plex、显示、启动、缓存、高级和日志。保存仍然只写 config.json
//! 中已知的持久字段，未知字段原样保留；命令行/环境变量仍优先于文件。

use std::cell::RefCell;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, FALSE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, DeleteObject, EndPaint, FillRect, GetStockObject, GetSysColorBrush,
    InvalidateRect, SetDIBitsToDevice, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_BTNFACE,
    COLOR_WINDOW, DEFAULT_CHARSET, DEFAULT_GUI_FONT, DEFAULT_PITCH, DIB_RGB_COLORS, FF_DONTCARE,
    FW_NORMAL, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::SS_LEFT;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem,
    GetMessageW, GetSystemMetrics, GetWindowTextW, IsIconic, IsWindow, KillTimer, LoadCursorW,
    MessageBoxW, PostMessageW, RegisterClassW, SendMessageW, SetForegroundWindow, SetTimer,
    SetWindowTextW, ShowWindow, TranslateMessage, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX,
    CBN_EDITCHANGE, CBN_SELCHANGE, CBS_DROPDOWN, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL,
    CB_SETCURSEL, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_PASSWORD, ES_READONLY, HICON,
    HMENU, IDC_ARROW, LBS_MULTIPLESEL, LBS_NOINTEGRALHEIGHT, LB_ADDSTRING, LB_GETCOUNT, LB_GETSEL,
    LB_GETTEXT, LB_RESETCONTENT, LB_SETSEL, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MSG,
    SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_RESTORE, SW_SHOW, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_PAINT, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CAPTION,
    WS_CHILD, WS_CLIPCHILDREN, WS_EX_APPWINDOW, WS_EX_CLIENTEDGE, WS_MINIMIZEBOX, WS_OVERLAPPED,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use crate::config::{
    self, Config, DisplayLayout, DisplaySettings, EditableSettings, MotionMode, Naming,
    SettingsOverrides, Transition,
};
use crate::logging::{log_info, log_warn};
use crate::plex::{Cache, Client, Section};
use crate::settings_preview::{load_preview_images, PreviewState};

const WINDOW_TITLE: &str = "Plex 海报屏保 - 设置";
const ID_LAYOUT: i32 = 1001;
const ID_TRANSITION: i32 = 1002;
const ID_TRANSITION_SECONDS: i32 = 1003;
const ID_DISPLAY_SECONDS: i32 = 1004;
const ID_MOTION: i32 = 1005;
const ID_KEN_BURNS: i32 = 1006;
const ID_PREVIEW: i32 = 1010;
const ID_HINT: i32 = 1011;

const ID_NAV_BASE: i32 = 2001;

const ID_PLEX_URL: i32 = 3001;
const ID_PLEX_TOKEN: i32 = 3002;
const ID_SHOW_TOKEN: i32 = 3003;
const ID_VERIFY_TLS: i32 = 3004;
const ID_TIMEOUT: i32 = 3005;
const ID_TEST: i32 = 3006;
const ID_TEST_STATUS: i32 = 3007;
const ID_LIBRARIES: i32 = 3008;

const ID_WATCH: i32 = 4001;
const ID_IDLE: i32 = 4002;
const ID_MOUSE_ONLY: i32 = 4003;
const ID_HOTKEY: i32 = 4004;

const ID_CACHE_DIR: i32 = 5001;
const ID_BROWSE: i32 = 5002;
const ID_CACHE_AUTO: i32 = 5003;
const ID_CACHE_NAMING: i32 = 5004;
const ID_CACHE_DAYS: i32 = 5005;
const ID_CACHE_MAX_MB: i32 = 5006;
const ID_TRANSCODE: i32 = 5007;
const ID_CACHE_INFO: i32 = 5008;

const ID_MOTION_FPS: i32 = 6001;
const ID_CONFIG_PATH: i32 = 6002;
const ID_OPEN_CONFIG: i32 = 6003;
const ID_OPEN_LOGS: i32 = 6004;

const ID_LOG_VIEW: i32 = 7001;

const ID_DEFAULTS: i32 = 1100;
const ID_CANCEL: i32 = 1101;
const ID_SAVE: i32 = 1102;
const ID_OVERRIDE: i32 = 1200;

const PREVIEW_TIMER: usize = 11;
const PREVIEW_INTERVAL_MS: u32 = 33;
const BST_CHECKED_VALUE: usize = 1;
const LOG_TAIL_LINES: usize = 300;
const LOG_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const WM_APP_TEST_DONE: u32 = WM_APP + 40;
const EM_SETSEL: u32 = 0x00B1;
const EM_SCROLLCARET: u32 = 0x00B7;
const EM_SETPASSWORDCHAR: u32 = 0x00CC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Display,
    Plex,
    Startup,
    Cache,
    Advanced,
    Logs,
}

const ALL_PAGES: [Page; 6] = [
    Page::Display,
    Page::Plex,
    Page::Startup,
    Page::Cache,
    Page::Advanced,
    Page::Logs,
];

impl Page {
    fn index(self) -> usize {
        match self {
            Self::Display => 0,
            Self::Plex => 1,
            Self::Startup => 2,
            Self::Cache => 3,
            Self::Advanced => 4,
            Self::Logs => 5,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Display => "显示",
            Self::Plex => "Plex 服务器",
            Self::Startup => "启动与热键",
            Self::Cache => "缓存",
            Self::Advanced => "高级",
            Self::Logs => "日志",
        }
    }

    fn nav_id(self) -> i32 {
        ID_NAV_BASE + self.index() as i32
    }
}

struct SettingsState {
    hwnd: HWND,
    preview_hwnd: HWND,
    log_view: HWND,
    path: Option<PathBuf>,
    font: HFONT,
    font_owned: bool,
    load_error: Option<String>,
    page: Page,
    page_controls: [Vec<HWND>; 6],
    preview: PreviewState,
    test_rx: Option<Receiver<Result<Vec<Section>, String>>>,
    sections: Vec<Section>,
    sections_loaded: bool,
    last_log_refresh: Instant,
    last_log_len: u64,
}

thread_local! {
    static STATE: RefCell<Option<SettingsState>> = const { RefCell::new(None) };
}

fn with_state<T>(f: impl FnOnce(&SettingsState) -> T) -> Option<T> {
    STATE.with(|cell| {
        let borrow = cell.try_borrow().ok()?;
        borrow.as_ref().map(f)
    })
}

fn with_state_mut<T>(f: impl FnOnce(&mut SettingsState) -> T) -> Option<T> {
    STATE.with(|cell| {
        let mut borrow = cell.try_borrow_mut().ok()?;
        borrow.as_mut().map(f)
    })
}

/// 托盘菜单入口。已有窗口时切回显示页并提到前面。
pub fn open(
    path: Option<PathBuf>,
    cache: Cache,
    icon: HICON,
    owner: HWND,
    overrides: SettingsOverrides,
) {
    if let Some(hwnd) = with_state(|state| state.hwnd) {
        if unsafe { IsWindow(hwnd).as_bool() } {
            with_state_mut(|state| state.show_page(Page::Display));
            let command = if unsafe { IsIconic(hwnd).as_bool() } {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            unsafe {
                let _ = ShowWindow(hwnd, command);
                let _ = SetForegroundWindow(hwnd);
            }
            return;
        }
        STATE.with(|cell| {
            let _ = cell.borrow_mut().take();
        });
    }
    if let Err(err) = create_window(path, cache, icon, owner, overrides, Page::Display) {
        log_warn!("打开设置失败: {err}");
        message_error(HWND::default(), &format!("打开设置失败：\n{err}"));
    }
}

/// 首次配置：没有 Token 时在启动前模态显示 Plex 页，保存/关闭后返回。
pub fn run_modal(
    path: Option<PathBuf>,
    cache: Cache,
    overrides: SettingsOverrides,
    page: Page,
) -> Result<(), String> {
    // 主线程在启动 App 前没有初始化 COM；缓存目录的“浏览…”用新版文件夹
    // 选择器时依赖 COM，这里先按 STA 初始化，窗口关闭后配对释放。
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let com_initialized = com.is_ok();
    let hwnd = match create_window(
        path,
        cache,
        HICON::default(),
        HWND::default(),
        overrides,
        page,
    ) {
        Ok(hwnd) => hwnd,
        Err(err) => {
            if com_initialized {
                unsafe {
                    CoUninitialize();
                }
            }
            return Err(err);
        }
    };
    let mut message = MSG::default();
    unsafe {
        while GetMessageW(&mut message, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
            if !IsWindow(hwnd).as_bool() {
                break;
            }
        }
    }
    if com_initialized {
        unsafe {
            CoUninitialize();
        }
    }
    Ok(())
}

pub fn close() {
    if let Some(hwnd) = with_state(|state| state.hwnd) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

fn create_window(
    path: Option<PathBuf>,
    cache: Cache,
    icon: HICON,
    owner: HWND,
    overrides: SettingsOverrides,
    initial_page: Page,
) -> Result<HWND, String> {
    let module =
        unsafe { GetModuleHandleW(None) }.map_err(|err| format!("获取模块句柄失败: {err}"))?;
    let instance = HINSTANCE(module.0);
    let cursor = unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW) }.unwrap_or_default();
    register_class(
        instance,
        icon,
        cursor,
        w!("PlexPosterScreensaverSettings"),
        Some(settings_proc),
        unsafe { GetSysColorBrush(COLOR_BTNFACE) },
    )?;
    register_class(
        instance,
        HICON::default(),
        cursor,
        w!("PlexPosterScreensaverSettingsPreview"),
        Some(preview_proc),
        unsafe { GetSysColorBrush(COLOR_WINDOW) },
    )?;

    let scale = (unsafe { GetDpiForSystem() } as f32 / 96.0).max(0.75);
    let s = |value: i32| ((value as f32) * scale).round() as i32;
    let client_w = s(900);
    let client_h = s(560);
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
    let mut outer = RECT {
        left: 0,
        top: 0,
        right: client_w,
        bottom: client_h,
    };
    unsafe { AdjustWindowRect(&mut outer, style, FALSE) }
        .map_err(|err| format!("计算窗口尺寸失败: {err}"))?;
    let outer_w = outer.right - outer.left;
    let outer_h = outer.bottom - outer.top;
    let x = ((unsafe { GetSystemMetrics(SM_CXSCREEN) } - outer_w) / 2).max(0);
    let y = ((unsafe { GetSystemMetrics(SM_CYSCREEN) } - outer_h) / 2).max(0);
    let title = wide(WINDOW_TITLE);

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            w!("PlexPosterScreensaverSettings"),
            PCWSTR(title.as_ptr()),
            style,
            x,
            y,
            outer_w,
            outer_h,
            owner,
            HMENU::default(),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建设置窗口失败: {err}"))?;

    let result = create_controls(hwnd, instance, s, path, cache, overrides);
    let state = match result {
        Ok(state) => state,
        Err(err) => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err(err);
        }
    };
    let load_error = state.load_error.clone();
    let initial = initial_page;
    STATE.with(|cell| *cell.borrow_mut() = Some(state));
    with_state_mut(|state| {
        state.show_page(initial);
        if initial == Page::Display {
            state.preview.update_frame(Instant::now());
        }
    });

    unsafe {
        let _ = SetTimer(hwnd, PREVIEW_TIMER, PREVIEW_INTERVAL_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }

    if let Some(err) = load_error {
        message_error(hwnd, &format!("读取配置失败，保存已禁用。\n{err}"));
    }
    Ok(hwnd)
}

fn register_class(
    instance: HINSTANCE,
    icon: HICON,
    cursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
    class_name: PCWSTR,
    proc: windows::Win32::UI::WindowsAndMessaging::WNDPROC,
    background: windows::Win32::Graphics::Gdi::HBRUSH,
) -> Result<(), String> {
    let class = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: instance,
        hIcon: icon,
        hCursor: cursor,
        hbrBackground: background,
        lpszClassName: class_name,
        ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        let err = unsafe { GetLastError() };
        if err.0 != ERROR_CLASS_ALREADY_EXISTS.0 {
            return Err(format!("注册设置窗口类失败: {err:?}"));
        }
    }
    Ok(())
}

#[allow(clippy::vec_init_then_push)]
fn create_controls(
    hwnd: HWND,
    instance: HINSTANCE,
    s: impl Fn(i32) -> i32,
    path: Option<PathBuf>,
    cache: Cache,
    overrides: SettingsOverrides,
) -> Result<SettingsState, String> {
    let font = unsafe {
        CreateFontW(
            -s(13),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
            w!("Segoe UI"),
        )
    };
    let (font, font_owned) = if font.0.is_null() {
        (HFONT(unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0), false)
    } else {
        (font, true)
    };
    set_font(hwnd, font);

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

    // 显示页
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
        "预览为加速循环。保存后下次启动生效；命令行参数仍会覆盖对应项。",
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
    pages[Page::Display.index()] = display;

    // Plex 页
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
    pages[Page::Plex.index()] = plex;

    // 启动与热键页
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
    pages[Page::Startup.index()] = startup;

    // 缓存页
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
        s(60),
        ID_CACHE_INFO,
        instance,
        font,
    )?);
    pages[Page::Cache.index()] = cache_page;

    // 高级页
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
    pages[Page::Advanced.index()] = advanced;

    // 日志页
    let log_view = create_log_view(hwnd, content_x, s(64), content_w, s(420), instance, font)?;
    pages[Page::Logs.index()] = vec![log_view];

    let right = s(900) - s(16);
    create_button(
        hwnd,
        "取消",
        right - s(90) * 2 - s(12),
        s(514),
        s(90),
        s(30),
        ID_CANCEL,
        instance,
        font,
    )?;
    create_button(
        hwnd,
        "保存",
        right - s(90),
        s(514),
        s(90),
        s(30),
        ID_SAVE,
        instance,
        font,
    )?;
    let (settings, load_error) = match config::load_editable_settings(path.as_deref()) {
        Ok(settings) => (settings, None),
        Err(err) => (EditableSettings::default(), Some(err)),
    };
    populate_controls(hwnd, &settings, path.as_deref(), &overrides);
    update_display_enabled(hwnd);
    update_cache_enabled(hwnd);
    let save = get_control(hwnd, ID_SAVE)?;
    if load_error.is_some() || path.is_none() {
        unsafe {
            let _ = EnableWindow(save, false);
        }
    }
    if path.is_none() && load_error.is_none() {
        set_window_text(
            hwnd,
            ID_HINT,
            "没有可写的配置文件位置，预览可用但无法保存。",
        );
    }

    let images = load_preview_images(&cache);
    let preview_state = PreviewState::new(
        settings.display.clone(),
        images,
        preview_w.max(1) as u32,
        preview_h.max(1) as u32,
    );

    Ok(SettingsState {
        hwnd,
        preview_hwnd: preview,
        log_view,
        path,
        font,
        font_owned,
        load_error,
        page: Page::Display,
        page_controls: pages,
        preview: preview_state,
        test_rx: None,
        sections: Vec::new(),
        sections_loaded: false,
        last_log_refresh: Instant::now(),
        last_log_len: 0,
    })
}

fn populate_controls(
    hwnd: HWND,
    settings: &EditableSettings,
    path: Option<&Path>,
    overrides: &SettingsOverrides,
) {
    populate_display(hwnd, &settings.display);
    set_window_text(hwnd, ID_PLEX_URL, &settings.plex_url);
    set_window_text(hwnd, ID_PLEX_TOKEN, &settings.plex_token);
    set_checked(hwnd, ID_VERIFY_TLS, settings.verify_tls);
    set_window_text(hwnd, ID_TIMEOUT, &settings.timeout.to_string());
    reset_library_list(hwnd, ID_LIBRARIES, &settings.libraries);
    let token_missing =
        settings.plex_token.trim().is_empty() && std::env::var_os("PLEX_TOKEN").is_none();
    set_window_text(
        hwnd,
        ID_TEST_STATUS,
        if token_missing {
            "请填写 Plex Token，然后点“测试连接”。"
        } else if settings.libraries.is_empty() {
            "未指定媒体库（使用全部）。可点“测试连接”加载实际媒体库。"
        } else {
            "已保存的媒体库已列出；可测试连接后重新选择。"
        },
    );

    set_checked(hwnd, ID_WATCH, settings.watch);
    set_window_text(hwnd, ID_IDLE, &format_number(settings.idle_minutes));
    set_checked(hwnd, ID_MOUSE_ONLY, settings.mouse_only);
    set_window_text(hwnd, ID_HOTKEY, &settings.hotkey);

    set_window_text(hwnd, ID_CACHE_DIR, &settings.cache_dir);
    set_checked(hwnd, ID_CACHE_AUTO, settings.cache_auto);
    set_combo_index(
        hwnd,
        ID_CACHE_NAMING,
        if settings.cache_naming == Naming::Title {
            0
        } else {
            1
        },
    );
    set_window_text(hwnd, ID_CACHE_DAYS, &settings.cache_days.to_string());
    set_window_text(hwnd, ID_CACHE_MAX_MB, &settings.cache_max_mb.to_string());
    set_checked(hwnd, ID_TRANSCODE, settings.transcode);
    set_window_text(
        hwnd,
        ID_CACHE_INFO,
        &format!(
            "保存的缓存目录：{}\n修改目录只影响下次启动；旧缓存不会自动搬移。",
            if settings.cache_auto {
                "auto（自动选择可写目录）".to_string()
            } else {
                settings.cache_dir.clone()
            }
        ),
    );

    set_window_text(hwnd, ID_MOTION_FPS, &settings.motion_fps.to_string());
    let config_text = match path {
        Some(path) => format!("当前配置文件：{}", path.display()),
        None => "未找到可写配置文件；保存时会尝试新建。".to_string(),
    };
    set_window_text(hwnd, ID_CONFIG_PATH, &config_text);

    if !overrides.is_empty() {
        set_window_text(
            hwnd,
            ID_OVERRIDE,
            &format!(
                "当前运行受启动参数/环境变量覆盖：{}。保存只改配置文件，不会改变本次运行。",
                overrides.summary()
            ),
        );
    }
}

fn populate_display(hwnd: HWND, settings: &DisplaySettings) {
    set_combo_index(
        hwnd,
        ID_LAYOUT,
        if settings.display_layout == DisplayLayout::Single {
            0
        } else {
            1
        },
    );
    set_combo_index(hwnd, ID_TRANSITION, transition_index(settings.transition));
    set_combo_index(
        hwnd,
        ID_MOTION,
        if settings.motion_mode == MotionMode::Animated {
            0
        } else {
            1
        },
    );
    set_window_text(
        hwnd,
        ID_TRANSITION_SECONDS,
        &format_seconds(settings.transition_seconds),
    );
    set_window_text(
        hwnd,
        ID_DISPLAY_SECONDS,
        &format_seconds(settings.display_seconds),
    );
    set_checked(hwnd, ID_KEN_BURNS, settings.ken_burns);
}

fn update_display_enabled(hwnd: HWND) {
    let layout = layout_from(combo_index(hwnd, ID_LAYOUT));
    let motion = motion_from(combo_index(hwnd, ID_MOTION));
    if let Ok(control) = get_control(hwnd, ID_KEN_BURNS) {
        unsafe {
            let _ = EnableWindow(
                control,
                layout == DisplayLayout::Single && motion == MotionMode::Animated,
            );
        }
    }
}

fn update_cache_enabled(hwnd: HWND) {
    let auto = checked(hwnd, ID_CACHE_AUTO);
    for id in [ID_CACHE_DIR, ID_BROWSE] {
        if let Ok(control) = get_control(hwnd, id) {
            unsafe {
                let _ = EnableWindow(control, !auto);
            }
        }
    }
}

impl SettingsState {
    fn show_page(&mut self, page: Page) {
        for controls in &self.page_controls {
            for control in controls {
                unsafe {
                    let _ = ShowWindow(*control, SW_HIDE);
                }
            }
        }
        for control in &self.page_controls[page.index()] {
            unsafe {
                let _ = ShowWindow(*control, SW_SHOW);
            }
        }
        self.page = page;
        for candidate in ALL_PAGES {
            if let Ok(button) = get_control(self.hwnd, candidate.nav_id()) {
                unsafe {
                    let _ = EnableWindow(button, candidate != page);
                }
            }
        }
        if page == Page::Logs {
            self.refresh_log(true);
        }
        if page == Page::Display {
            self.invalidate_preview();
        }
    }

    fn invalidate_preview(&self) {
        unsafe {
            let _ = InvalidateRect(self.preview_hwnd, None, FALSE);
        }
    }

    fn on_display_changed(&mut self) {
        update_display_enabled(self.hwnd);
        if let Ok(display) = read_display(self.hwnd) {
            self.preview.set_settings(display);
            self.invalidate_preview();
        }
    }

    fn reset_display_defaults(&mut self) {
        let defaults = EditableSettings::default();
        populate_display(self.hwnd, &defaults.display);
        update_display_enabled(self.hwnd);
        self.preview.set_settings(defaults.display);
        self.invalidate_preview();
    }

    fn toggle_token(&mut self) {
        let show = checked(self.hwnd, ID_SHOW_TOKEN);
        if let Ok(token) = get_control(self.hwnd, ID_PLEX_TOKEN) {
            unsafe {
                let _ = SendMessageW(
                    token,
                    EM_SETPASSWORDCHAR,
                    WPARAM(if show { 0 } else { 0x25CF }),
                    LPARAM(0),
                );
                let _ = InvalidateRect(token, None, FALSE);
            }
        }
    }

    fn browse_cache_dir(&mut self) {
        let Some(path) = choose_folder(self.hwnd) else {
            return;
        };
        set_window_text(self.hwnd, ID_CACHE_DIR, &path.to_string_lossy());
        set_checked(self.hwnd, ID_CACHE_AUTO, false);
        update_cache_enabled(self.hwnd);
    }

    fn start_test(&mut self) {
        if self.test_rx.is_some() {
            return;
        }
        let url = text_control(self.hwnd, ID_PLEX_URL)
            .trim()
            .trim_end_matches('/')
            .to_string();
        let field_token = text_control(self.hwnd, ID_PLEX_TOKEN);
        let token = if field_token.trim().is_empty() {
            std::env::var("PLEX_TOKEN").unwrap_or_default()
        } else {
            field_token
        };
        if url.is_empty() || token.trim().is_empty() {
            set_window_text(self.hwnd, ID_TEST_STATUS, "请先填写服务器地址和 Token。");
            return;
        }
        let timeout = match parse_u64(&text_control(self.hwnd, ID_TIMEOUT)) {
            Ok(value) if (1..=600).contains(&value) => value,
            _ => {
                set_window_text(self.hwnd, ID_TEST_STATUS, "连接超时必须是 1–600 秒。");
                return;
            }
        };
        let verify_tls = checked(self.hwnd, ID_VERIFY_TLS);
        let (tx, rx) = mpsc::channel();
        self.test_rx = Some(rx);
        set_window_text(self.hwnd, ID_TEST_STATUS, "正在连接 Plex…");
        if let Ok(button) = get_control(self.hwnd, ID_TEST) {
            unsafe {
                let _ = EnableWindow(button, false);
            }
        }
        let hwnd = self.hwnd.0 as isize;
        let spawned = thread::Builder::new()
            .name("plex-settings-test".to_string())
            .spawn(move || {
                let cfg = Config {
                    url,
                    token: token.trim().to_string(),
                    verify_tls,
                    timeout,
                    ..Config::default()
                };
                let result = Client::new(&cfg).sections();
                let _ = tx.send(result);
                unsafe {
                    let _ = PostMessageW(
                        HWND(hwnd as *mut c_void),
                        WM_APP_TEST_DONE,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            });
        if spawned.is_err() {
            self.test_rx = None;
            set_window_text(self.hwnd, ID_TEST_STATUS, "无法启动连接线程，请重试。");
            if let Ok(button) = get_control(self.hwnd, ID_TEST) {
                unsafe {
                    let _ = EnableWindow(button, true);
                }
            }
        }
    }

    fn finish_test(&mut self) {
        let Some(rx) = self.test_rx.take() else {
            return;
        };
        if let Ok(button) = get_control(self.hwnd, ID_TEST) {
            unsafe {
                let _ = EnableWindow(button, true);
            }
        }
        match rx.try_recv() {
            Ok(Ok(sections)) => {
                let selected = selected_library_titles(self.hwnd, ID_LIBRARIES);
                let count = sections.len();
                self.sections = sections.clone();
                self.sections_loaded = true;
                fill_library_list(self.hwnd, &sections, &selected);
                set_window_text(
                    self.hwnd,
                    ID_TEST_STATUS,
                    &if count == 0 {
                        "连接成功，但服务器上没有电影或剧集媒体库。".to_string()
                    } else {
                        format!("连接成功，找到 {count} 个媒体库。已按原选择勾选。")
                    },
                );
            }
            Ok(Err(err)) => {
                set_window_text(self.hwnd, ID_TEST_STATUS, &format!("连接失败：{err}"));
            }
            Err(_) => {
                set_window_text(self.hwnd, ID_TEST_STATUS, "连接线程意外退出，请重试。");
            }
        }
    }

    fn refresh_log(&mut self, force: bool) {
        let path = crate::logging::log_dir().join("app.log");
        let file_len = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if !force && file_len == self.last_log_len {
            return;
        }
        self.last_log_len = file_len;
        let text = read_log_file(&path, LOG_TAIL_LINES);
        set_window_text(self.hwnd, ID_LOG_VIEW, &text);
        let len = text.encode_utf16().count();
        unsafe {
            let _ = SendMessageW(self.log_view, EM_SETSEL, WPARAM(len), LPARAM(len as isize));
            let _ = SendMessageW(self.log_view, EM_SCROLLCARET, WPARAM(0), LPARAM(0));
        }
    }
}

fn read_display(hwnd: HWND) -> Result<DisplaySettings, String> {
    let transition_seconds = parse_seconds(&text_control(hwnd, ID_TRANSITION_SECONDS))
        .ok_or_else(|| "切换时长必须是数字（0 或 0.1–3.0 秒）".to_string())?;
    let display_seconds = parse_seconds(&text_control(hwnd, ID_DISPLAY_SECONDS))
        .ok_or_else(|| "每张停留时间必须是数字（0.1–3600 秒）".to_string())?;
    Ok(DisplaySettings {
        display_layout: layout_from(combo_index(hwnd, ID_LAYOUT)),
        transition: transition_from(combo_index(hwnd, ID_TRANSITION)),
        transition_seconds,
        display_seconds,
        motion_mode: motion_from(combo_index(hwnd, ID_MOTION)),
        ken_burns: checked(hwnd, ID_KEN_BURNS),
    })
}

fn read_editable(hwnd: HWND, sections_loaded: bool) -> Result<EditableSettings, String> {
    let display = read_display(hwnd)?;
    let timeout =
        parse_u64(&text_control(hwnd, ID_TIMEOUT)).map_err(|_| "连接超时必须是整数".to_string())?;
    let mut libraries = selected_library_titles(hwnd, ID_LIBRARIES);
    if sections_loaded {
        let count = list_count(hwnd, ID_LIBRARIES);
        if libraries.len() == count {
            libraries.clear();
        }
    }
    let cache_auto = checked(hwnd, ID_CACHE_AUTO);
    let cache_naming = if combo_index(hwnd, ID_CACHE_NAMING) == Some(1) {
        Naming::Key
    } else {
        Naming::Title
    };
    let cache_days = text_control(hwnd, ID_CACHE_DAYS)
        .trim()
        .parse::<i64>()
        .map_err(|_| "缓存保留天数必须是整数".to_string())?;
    let cache_max_mb = text_control(hwnd, ID_CACHE_MAX_MB)
        .trim()
        .parse::<u64>()
        .map_err(|_| "缓存容量必须是整数 MB".to_string())?;
    let idle_minutes = parse_number(&text_control(hwnd, ID_IDLE))
        .ok_or_else(|| "空闲时间必须是数字（分钟）".to_string())?;
    let motion_fps = text_control(hwnd, ID_MOTION_FPS)
        .trim()
        .parse::<u32>()
        .map_err(|_| "动效帧率必须是整数".to_string())?;

    let settings = EditableSettings {
        plex_url: text_control(hwnd, ID_PLEX_URL).trim().to_string(),
        plex_token: text_control(hwnd, ID_PLEX_TOKEN).trim().to_string(),
        libraries,
        verify_tls: checked(hwnd, ID_VERIFY_TLS),
        timeout,
        display,
        motion_fps,
        watch: checked(hwnd, ID_WATCH),
        idle_minutes,
        mouse_only: checked(hwnd, ID_MOUSE_ONLY),
        hotkey: text_control(hwnd, ID_HOTKEY).trim().to_string(),
        cache_dir: text_control(hwnd, ID_CACHE_DIR).trim().to_string(),
        cache_auto,
        cache_naming,
        cache_days,
        cache_max_mb,
        transcode: checked(hwnd, ID_TRANSCODE),
    };
    let require_token = std::env::var_os("PLEX_TOKEN").is_none();
    settings.validate(require_token)?;
    Ok(settings)
}

fn save_settings(hwnd: HWND) {
    let Some((path, load_error, settings)) = with_state(|state| {
        (
            state.path.clone(),
            state.load_error.clone(),
            read_editable(state.hwnd, state.sections_loaded),
        )
    }) else {
        return;
    };
    if let Some(err) = load_error {
        message_error(hwnd, &err);
        return;
    }
    let settings = match settings {
        Ok(settings) => settings,
        Err(err) => {
            message_error(hwnd, &err);
            return;
        }
    };
    match config::save_editable_settings(path.as_deref(), &settings) {
        Ok(()) => {
            log_info!(
                "设置已保存: {}",
                path.as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(无配置文件)".to_string())
            );
            message_info(
                hwnd,
                "设置已保存，下次启动屏保时生效。\n命令行参数/环境变量仍会覆盖对应项。",
            );
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        Err(err) => message_error(hwnd, &err),
    }
}

fn message_error(hwnd: HWND, text: &str) {
    message_box(hwnd, text, MB_OK | MB_ICONERROR);
}

fn message_info(hwnd: HWND, text: &str) {
    message_box(hwnd, text, MB_OK | MB_ICONINFORMATION);
}

fn message_box(
    hwnd: HWND,
    text: &str,
    flags: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) {
    let text = wide(text);
    let caption = wide("设置");
    unsafe {
        MessageBoxW(hwnd, PCWSTR(text.as_ptr()), PCWSTR(caption.as_ptr()), flags);
    }
}

unsafe extern "system" fn settings_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as i32;
            let code = ((wparam.0 >> 16) & 0xffff) as u32;
            if (ID_NAV_BASE..ID_NAV_BASE + 6).contains(&id) && code == 0 {
                let page = ALL_PAGES[(id - ID_NAV_BASE) as usize];
                with_state_mut(|state| state.show_page(page));
            } else {
                match id {
                    ID_DEFAULTS if code == 0 => {
                        with_state_mut(|state| state.reset_display_defaults());
                    }
                    ID_CANCEL if code == 0 => {
                        let _ = unsafe { DestroyWindow(hwnd) };
                    }
                    ID_SAVE if code == 0 => save_settings(hwnd),
                    ID_LAYOUT | ID_TRANSITION | ID_MOTION if code == CBN_SELCHANGE => {
                        with_state_mut(|state| state.on_display_changed());
                    }
                    ID_TRANSITION_SECONDS | ID_DISPLAY_SECONDS
                        if code == CBN_SELCHANGE || code == CBN_EDITCHANGE =>
                    {
                        with_state_mut(|state| state.on_display_changed());
                    }
                    ID_KEN_BURNS if code == 0 => {
                        with_state_mut(|state| state.on_display_changed());
                    }
                    ID_SHOW_TOKEN if code == 0 => {
                        with_state_mut(|state| state.toggle_token());
                    }
                    ID_TEST if code == 0 => {
                        with_state_mut(|state| state.start_test());
                    }
                    ID_CACHE_AUTO if code == 0 => {
                        with_state_mut(|state| {
                            update_cache_enabled(state.hwnd);
                        });
                    }
                    ID_BROWSE if code == 0 => {
                        with_state_mut(|state| state.browse_cache_dir());
                    }
                    ID_OPEN_CONFIG if code == 0 => {
                        let path = with_state(|state| state.path.clone()).flatten();
                        match path {
                            Some(path) => {
                                if path.exists() {
                                    crate::platform::open_path(&path);
                                } else {
                                    message_info(
                                        hwnd,
                                        &format!("配置文件尚未创建：{}", path.display()),
                                    );
                                }
                            }
                            None => message_info(hwnd, "没有找到配置文件位置。"),
                        }
                    }
                    ID_OPEN_LOGS if code == 0 => {
                        crate::platform::open_path(&crate::logging::log_dir());
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == PREVIEW_TIMER {
                let preview = with_state_mut(|state| match state.page {
                    Page::Display => {
                        state.preview.update_frame(Instant::now());
                        Some(state.preview_hwnd)
                    }
                    Page::Logs => {
                        if state.last_log_refresh.elapsed() >= LOG_REFRESH_INTERVAL {
                            state.last_log_refresh = Instant::now();
                            state.refresh_log(false);
                        }
                        None
                    }
                    _ => None,
                });
                if let Some(preview) = preview.flatten() {
                    unsafe {
                        let _ = InvalidateRect(preview, None, FALSE);
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_TEST_DONE => {
            with_state_mut(|state| state.finish_test());
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 == 0x1B {
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = KillTimer(hwnd, PREVIEW_TIMER);
            }
            let state = STATE.with(|cell| {
                let mut borrow = cell.borrow_mut();
                borrow.take()
            });
            if let Some(state) = state {
                if state.font_owned && !state.font.0.is_null() {
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(state.font.0));
                    }
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe extern "system" fn preview_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
            let painted = with_state(|state| {
                if state.preview_hwnd != hwnd {
                    return false;
                }
                let (width, height) = (state.preview.width, state.preview.height);
                if width == 0 || height == 0 {
                    return false;
                }
                let bmi = state.preview.bitmap_info();
                unsafe {
                    SetDIBitsToDevice(
                        hdc,
                        0,
                        0,
                        width,
                        height,
                        0,
                        0,
                        0,
                        height,
                        state.preview.frame.as_ptr() as *const c_void,
                        &bmi,
                        DIB_RGB_COLORS,
                    ) != 0
                }
            })
            .unwrap_or(false);
            if !painted {
                unsafe {
                    FillRect(hdc, &paint.rcPaint, GetSysColorBrush(COLOR_BTNFACE));
                }
            }
            unsafe {
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[allow(clippy::too_many_arguments)]
fn create_static(
    parent: HWND,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    instance: HINSTANCE,
    font: HFONT,
) -> Result<HWND, String> {
    create_static_id(parent, text, x, y, w, h, 0, instance, font)
}

#[allow(clippy::too_many_arguments)]
fn create_static_id(
    parent: HWND,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    instance: HINSTANCE,
    font: HFONT,
) -> Result<HWND, String> {
    let text = wide(text);
    let control = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR(text.as_ptr()),
            WS_CHILD
                | WS_VISIBLE
                | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(SS_LEFT.0),
            x,
            y,
            w,
            h,
            parent,
            if id == 0 {
                HMENU::default()
            } else {
                HMENU(id as usize as *mut c_void)
            },
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建标签失败: {err}"))?;
    set_font(control, font);
    Ok(control)
}

#[allow(clippy::too_many_arguments)]
fn create_edit(
    parent: HWND,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    password: bool,
    readonly: bool,
    multiline: bool,
    instance: HINSTANCE,
    font: HFONT,
) -> Result<HWND, String> {
    let mut style = WS_CHILD
        | WS_VISIBLE
        | WS_TABSTOP
        | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_AUTOHSCROLL as u32);
    if password {
        style |= windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_PASSWORD as u32);
    }
    if readonly {
        style |= windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_READONLY as u32);
    }
    if multiline {
        style |= WS_VSCROLL
            | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                (ES_MULTILINE | ES_AUTOVSCROLL) as u32,
            );
    }
    let text = wide(text);
    let control = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            PCWSTR(text.as_ptr()),
            style,
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as usize as *mut c_void),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建输入框失败: {err}"))?;
    set_font(control, font);
    Ok(control)
}

fn create_log_view(
    parent: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    instance: HINSTANCE,
    font: HFONT,
) -> Result<HWND, String> {
    create_edit(
        parent,
        "",
        x,
        y,
        w,
        h,
        ID_LOG_VIEW,
        false,
        true,
        true,
        instance,
        font,
    )
}

#[allow(clippy::too_many_arguments)]
fn create_checkbox(
    parent: HWND,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    instance: HINSTANCE,
    font: HFONT,
) -> Result<HWND, String> {
    let text = wide(text);
    let control = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(text.as_ptr()),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as usize as *mut c_void),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建复选框失败: {err}"))?;
    set_font(control, font);
    Ok(control)
}

#[allow(clippy::too_many_arguments)]
fn create_button(
    parent: HWND,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    instance: HINSTANCE,
    font: HFONT,
) -> Result<HWND, String> {
    let text = wide(text);
    let control = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as usize as *mut c_void),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建按钮失败: {err}"))?;
    set_font(control, font);
    Ok(control)
}

#[allow(clippy::too_many_arguments)]
fn create_combo(
    parent: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    editable: bool,
    instance: HINSTANCE,
    font: HFONT,
) -> Result<HWND, String> {
    let style = if editable {
        CBS_DROPDOWN as u32
    } else {
        CBS_DROPDOWNLIST as u32
    };
    let control = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            w!("COMBOBOX"),
            w!(""),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as usize as *mut c_void),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建下拉框失败: {err}"))?;
    set_font(control, font);
    Ok(control)
}

#[allow(clippy::too_many_arguments)]
fn create_listbox(
    parent: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    instance: HINSTANCE,
    font: HFONT,
) -> Result<HWND, String> {
    let control = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("LISTBOX"),
            w!(""),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                    (LBS_MULTIPLESEL | LBS_NOINTEGRALHEIGHT) as u32,
                ),
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as usize as *mut c_void),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建媒体库列表失败: {err}"))?;
    set_font(control, font);
    Ok(control)
}

fn add_combo_items(combo: HWND, items: &[&str]) {
    for item in items {
        let wide = wide(item);
        unsafe {
            let _ = SendMessageW(
                combo,
                CB_ADDSTRING,
                WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
            );
        }
    }
}

fn set_font(control: HWND, font: HFONT) {
    unsafe {
        let _ = SendMessageW(control, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

fn get_control(hwnd: HWND, id: i32) -> Result<HWND, String> {
    unsafe { GetDlgItem(hwnd, id) }.map_err(|err| format!("找不到控件 {id}: {err}"))
}

fn set_window_text(hwnd: HWND, id: i32, text: &str) {
    if let Ok(control) = get_control(hwnd, id) {
        let wide = wide(text);
        unsafe {
            let _ = SetWindowTextW(control, PCWSTR(wide.as_ptr()));
        }
    }
}

fn text_control(hwnd: HWND, id: i32) -> String {
    let Ok(control) = get_control(hwnd, id) else {
        return String::new();
    };
    let mut buffer = [0u16; 512];
    let len = unsafe { GetWindowTextW(control, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

fn set_combo_index(hwnd: HWND, id: i32, index: usize) {
    if let Ok(control) = get_control(hwnd, id) {
        unsafe {
            let _ = SendMessageW(control, CB_SETCURSEL, WPARAM(index), LPARAM(0));
        }
    }
}

fn combo_index(hwnd: HWND, id: i32) -> Option<usize> {
    let control = get_control(hwnd, id).ok()?;
    let result = unsafe { SendMessageW(control, CB_GETCURSEL, WPARAM(0), LPARAM(0)) };
    if result.0 < 0 {
        None
    } else {
        Some(result.0 as usize)
    }
}

fn set_checked(hwnd: HWND, id: i32, checked: bool) {
    if let Ok(control) = get_control(hwnd, id) {
        unsafe {
            let _ = SendMessageW(
                control,
                BM_SETCHECK,
                WPARAM(if checked { BST_CHECKED_VALUE } else { 0 }),
                LPARAM(0),
            );
        }
    }
}

fn checked(hwnd: HWND, id: i32) -> bool {
    let Ok(control) = get_control(hwnd, id) else {
        return false;
    };
    unsafe {
        SendMessageW(control, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == BST_CHECKED_VALUE as isize
    }
}

fn reset_library_list(hwnd: HWND, id: i32, libraries: &[String]) {
    let Ok(list) = get_control(hwnd, id) else {
        return;
    };
    unsafe {
        let _ = SendMessageW(list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
    }
    for title in libraries {
        let wide = wide(title);
        unsafe {
            let _ = SendMessageW(
                list,
                LB_ADDSTRING,
                WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
            );
            let index = SendMessageW(list, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 - 1;
            let _ = SendMessageW(list, LB_SETSEL, WPARAM(1), LPARAM(index));
        }
    }
}

fn list_count(hwnd: HWND, id: i32) -> usize {
    let Ok(list) = get_control(hwnd, id) else {
        return 0;
    };
    unsafe {
        SendMessageW(list, LB_GETCOUNT, WPARAM(0), LPARAM(0))
            .0
            .max(0) as usize
    }
}

fn selected_library_titles(hwnd: HWND, id: i32) -> Vec<String> {
    let Ok(list) = get_control(hwnd, id) else {
        return Vec::new();
    };
    let count = unsafe {
        SendMessageW(list, LB_GETCOUNT, WPARAM(0), LPARAM(0))
            .0
            .max(0) as usize
    };
    let mut selected = Vec::new();
    for index in 0..count {
        let is_selected = unsafe { SendMessageW(list, LB_GETSEL, WPARAM(index), LPARAM(0)).0 };
        if is_selected <= 0 {
            continue;
        }
        let mut buffer = [0u16; 256];
        unsafe {
            SendMessageW(
                list,
                LB_GETTEXT,
                WPARAM(index),
                LPARAM(buffer.as_mut_ptr() as isize),
            );
        }
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        selected.push(String::from_utf16_lossy(&buffer[..len]));
    }
    selected
}

fn fill_library_list(hwnd: HWND, sections: &[Section], previous: &[String]) {
    let Ok(list) = get_control(hwnd, ID_LIBRARIES) else {
        return;
    };
    unsafe {
        let _ = SendMessageW(list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
    }
    let select_all = previous.is_empty();
    let mut matched = false;
    for section in sections {
        let wide = wide(&section.title);
        unsafe {
            let _ = SendMessageW(
                list,
                LB_ADDSTRING,
                WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
            );
        }
        let index = list_count(hwnd, ID_LIBRARIES).saturating_sub(1) as isize;
        let selected = select_all || previous.iter().any(|title| title == &section.title);
        matched |= selected;
        unsafe {
            let _ = SendMessageW(
                list,
                LB_SETSEL,
                WPARAM(usize::from(selected)),
                LPARAM(index),
            );
        }
    }
    if !select_all && !matched {
        for index in 0..sections.len() {
            unsafe {
                let _ = SendMessageW(list, LB_SETSEL, WPARAM(1), LPARAM(index as isize));
            }
        }
    }
}

fn choose_folder(owner: HWND) -> Option<PathBuf> {
    let mut display = [0u16; 260];
    let info = BROWSEINFOW {
        hwndOwner: owner,
        pszDisplayName: PWSTR(display.as_mut_ptr()),
        lpszTitle: w!("选择缓存目录"),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        ..Default::default()
    };
    let pidl = unsafe { SHBrowseForFolderW(&info) };
    if pidl.is_null() {
        return None;
    }
    let mut path = [0u16; 260];
    let ok = unsafe { SHGetPathFromIDListW(pidl, &mut path) }.as_bool();
    unsafe {
        CoTaskMemFree(Some(pidl as *const c_void));
    }
    if !ok {
        return None;
    }
    let len = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    Some(PathBuf::from(String::from_utf16_lossy(&path[..len])))
}

fn layout_from(index: Option<usize>) -> DisplayLayout {
    if index == Some(1) {
        DisplayLayout::Wall
    } else {
        DisplayLayout::Single
    }
}

fn transition_index(transition: Transition) -> usize {
    match transition {
        Transition::Fade => 0,
        Transition::SlideLeft => 1,
        Transition::SlideRight => 2,
        Transition::SlideUp => 3,
        Transition::SlideDown => 4,
        Transition::Random => 5,
    }
}

fn transition_from(index: Option<usize>) -> Transition {
    match index {
        Some(1) => Transition::SlideLeft,
        Some(2) => Transition::SlideRight,
        Some(3) => Transition::SlideUp,
        Some(4) => Transition::SlideDown,
        Some(5) => Transition::Random,
        _ => Transition::Fade,
    }
}

fn motion_from(index: Option<usize>) -> MotionMode {
    if index == Some(1) {
        MotionMode::Static
    } else {
        MotionMode::Animated
    }
}

fn format_seconds(value: f64) -> String {
    format_number(value)
}

fn format_number(value: f64) -> String {
    let text = format!("{value:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() {
        "0".to_string()
    } else {
        text.to_string()
    }
}

fn parse_seconds(text: &str) -> Option<f64> {
    parse_number(text.trim().trim_end_matches('秒').trim())
}

fn parse_number(text: &str) -> Option<f64> {
    text.replace(',', ".").parse::<f64>().ok()
}

fn parse_u64(text: &str) -> Result<u64, std::num::ParseIntError> {
    text.trim().parse::<u64>()
}

fn read_log_file(path: &Path, limit: usize) -> String {
    match fs::read_to_string(path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(limit);
            let mut out = format!("日志文件: {}\r\n", path.display());
            out.push_str(&format!(
                "最近 {} 行；打开期间每秒刷新。\r\n\r\n",
                lines.len() - start
            ));
            if lines.is_empty() {
                out.push_str("（日志为空）\r\n");
            } else {
                for line in &lines[start..] {
                    out.push_str(line);
                    out.push_str("\r\n");
                }
            }
            out
        }
        Err(err) => format!(
            "日志文件: {}\r\n\r\n无法读取日志: {err}\r\n",
            path.display()
        ),
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_seconds_round_trip() {
        assert_eq!(format_seconds(0.6), "0.6");
        assert_eq!(format_seconds(1.0), "1");
        assert_eq!(format_seconds(0.25), "0.25");
        assert_eq!(format_seconds(0.0), "0");
        assert_eq!(parse_seconds("0.6"), Some(0.6));
        assert_eq!(parse_seconds(" 2,5 秒"), Some(2.5));
        assert_eq!(parse_seconds("abc"), None);
    }

    #[test]
    fn log_tail_keeps_only_last_lines() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("settings-log-tests")
            .join(format!("{}-{stamp}", std::process::id()));
        let path = dir.join("app.log");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();

        let text = read_log_file(&path, 2);
        assert!(text.contains("three"));
        assert!(text.contains("four"));
        assert!(!text.contains("one"));
        assert!(text.contains("最近 2 行"));
        assert!(read_log_file(&dir.join("missing.log"), 2).contains("无法读取日志"));
        let _ = fs::remove_dir_all(&dir);
    }
}

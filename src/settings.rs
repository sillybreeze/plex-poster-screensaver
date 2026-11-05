//! 托盘菜单打开的显示设置窗口。
//!
//! 界面使用原生 Win32 控件，不引入额外 GUI 依赖；右侧预览在 CPU 上按
//! 当前布局/切换效果/动效模式合成，循环播放两张示例画面。
//! 保存只覆盖常用字段，`config.json` 里的其余高级项保持原样。

use std::cell::RefCell;
use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use image::RgbImage;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, FALSE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, DeleteObject, EndPaint, FillRect, GetStockObject, GetSysColorBrush,
    InvalidateRect, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, COLOR_BTNFACE, COLOR_WINDOW, DEFAULT_CHARSET, DEFAULT_GUI_FONT,
    DEFAULT_PITCH, DIB_RGB_COLORS, FF_DONTCARE, FW_NORMAL, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::SS_LEFT;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW, GetDlgItem,
    GetSystemMetrics, GetWindowTextW, IsIconic, IsWindow, KillTimer, LoadCursorW, MessageBoxW,
    RegisterClassW, SendMessageW, SetForegroundWindow, SetTimer, SetWindowTextW, ShowWindow,
    BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, CBN_EDITCHANGE, CBN_SELCHANGE, CBS_DROPDOWN,
    CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, ES_AUTOVSCROLL, ES_MULTILINE,
    ES_READONLY, HICON, HMENU, IDC_ARROW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, SM_CXSCREEN,
    SM_CYSCREEN, SW_HIDE, SW_RESTORE, SW_SHOW, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_PAINT, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN,
    WS_EX_APPWINDOW, WS_EX_CLIENTEDGE, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE, WS_VSCROLL,
};

use crate::config::{
    self, DisplayLayout, DisplaySettings, MotionMode, Transition, MAX_TRANSITION_SECONDS,
};
use crate::layout::WALL_POSTERS;
use crate::logging::{log_info, log_warn};
use crate::plex::Cache;
use crate::render::{self, Frame, DIRECTIONS};

const WINDOW_TITLE: &str = "Plex 海报屏保 - 显示设置";

const ID_LAYOUT: i32 = 1001;
const ID_TRANSITION: i32 = 1002;
const ID_TRANSITION_SECONDS: i32 = 1003;
const ID_DISPLAY_SECONDS: i32 = 1004;
const ID_MOTION: i32 = 1005;
const ID_KEN_BURNS: i32 = 1006;
const ID_PREVIEW: i32 = 1010;
const ID_LOG_VIEW: i32 = 1012;
const ID_DEFAULTS: i32 = 1100;
const ID_CANCEL: i32 = 1101;
const ID_SAVE: i32 = 1102;
const ID_LOG_BUTTON: i32 = 1103;

const PREVIEW_TIMER: usize = 11;
const PREVIEW_INTERVAL_MS: u32 = 33;
const BST_CHECKED_VALUE: usize = 1;
const PREVIEW_MAX_HOLD: f64 = 4.0;
const LOG_TAIL_LINES: usize = 300;
const LOG_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const EM_SETSEL: u32 = 0x00B1;
const EM_SCROLLCARET: u32 = 0x00B7;

const LAYOUT_LABELS: [&str; 2] = ["单张海报", "6 张海报墙（3×2）"];
const TRANSITION_LABELS: [&str; 6] = [
    "淡化",
    "从左侧滑入",
    "从右侧滑入",
    "从上方滑入",
    "从下方滑入",
    "随机方向",
];
const MOTION_LABELS: [&str; 2] = ["动画", "静止"];
const TRANSITION_PRESETS: [&str; 11] = [
    "0.2", "0.3", "0.4", "0.6", "0.8", "1.0", "1.2", "1.5", "2.0", "2.5", "3.0",
];
const DISPLAY_PRESETS: [&str; 7] = ["3", "5", "8", "10", "15", "30", "60"];

const HINT_TEXT: &str =
    "预览为加速循环。保存后下次启动屏保时生效；命令行参数仍会在启动时覆盖对应项。";

struct PreviewState {
    images: Vec<RgbImage>,
    scenes: Option<[Frame; 2]>,
    settings: DisplaySettings,
    width: u32,
    height: u32,
    frame: Vec<u32>,
    scratch: Vec<u32>,
    epoch: Instant,
}

struct SettingsState {
    hwnd: HWND,
    preview_hwnd: HWND,
    log_view: HWND,
    log_button: HWND,
    hint: HWND,
    path: Option<PathBuf>,
    font: HFONT,
    font_owned: bool,
    load_error: Option<String>,
    log_mode: bool,
    last_log_refresh: Instant,
    last_log_len: u64,
    preview: PreviewState,
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

/// 托盘菜单入口：重复打开时把已有窗口提到前面。
pub fn open(path: Option<PathBuf>, cache: Cache, icon: HICON, owner: HWND) {
    if let Some(hwnd) = with_state(|state| state.hwnd) {
        if unsafe { IsWindow(hwnd).as_bool() } {
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
    if let Err(err) = open_inner(path, cache, icon, owner) {
        log_warn!("打开显示设置失败: {err}");
        message_error(HWND::default(), &format!("打开显示设置失败：\n{err}"));
    }
}

/// 退出/托盘清理时关闭设置窗口。
pub fn close() {
    if let Some(hwnd) = with_state(|state| state.hwnd) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

fn open_inner(path: Option<PathBuf>, cache: Cache, icon: HICON, owner: HWND) -> Result<(), String> {
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
    let client_w = s(780);
    let client_h = s(396);
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

    let hwnd = unsafe {
        let title = wide(WINDOW_TITLE);
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

    let result = create_controls(hwnd, instance, s, path, cache);
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
    STATE.with(|cell| *cell.borrow_mut() = Some(state));
    with_state_mut(|state| state.preview.update_frame(Instant::now()));

    unsafe {
        let _ = SetTimer(hwnd, PREVIEW_TIMER, PREVIEW_INTERVAL_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }

    if let Some(err) = load_error {
        message_error(hwnd, &format!("读取当前配置失败，保存已禁用。\n{err}"));
    }
    Ok(())
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

fn create_controls(
    hwnd: HWND,
    instance: HINSTANCE,
    s: impl Fn(i32) -> i32,
    path: Option<PathBuf>,
    cache: Cache,
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
        let stock = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
        (HFONT(stock.0), false)
    } else {
        (font, true)
    };
    set_font(hwnd, font);

    let form_x = s(16);
    let label_w = s(112);
    let control_x = form_x + label_w + s(8);
    let control_w = s(134);
    let row_y = [s(24), s(64), s(104), s(144), s(184)];
    let preview_x = s(282);
    let preview_w = s(480);
    let preview_h = ((preview_w as f32) * 9.0 / 16.0).round() as i32;
    let preview_y = s(20);

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
        create_static(
            hwnd,
            label,
            form_x,
            row_y[index],
            label_w,
            s(20),
            instance,
            font,
        )?;
    }

    let layout_combo = create_combo(
        hwnd,
        control_x,
        row_y[0],
        control_w,
        s(180),
        ID_LAYOUT,
        false,
        instance,
        font,
    )?;
    add_combo_items(layout_combo, &LAYOUT_LABELS);

    let transition_combo = create_combo(
        hwnd,
        control_x,
        row_y[1],
        control_w,
        s(180),
        ID_TRANSITION,
        false,
        instance,
        font,
    )?;
    add_combo_items(transition_combo, &TRANSITION_LABELS);

    let transition_seconds_combo = create_combo(
        hwnd,
        control_x,
        row_y[2],
        control_w,
        s(180),
        ID_TRANSITION_SECONDS,
        true,
        instance,
        font,
    )?;
    add_combo_items(transition_seconds_combo, &TRANSITION_PRESETS);

    let display_seconds_combo = create_combo(
        hwnd,
        control_x,
        row_y[3],
        control_w,
        s(180),
        ID_DISPLAY_SECONDS,
        true,
        instance,
        font,
    )?;
    add_combo_items(display_seconds_combo, &DISPLAY_PRESETS);

    let motion_combo = create_combo(
        hwnd,
        control_x,
        row_y[4],
        control_w,
        s(180),
        ID_MOTION,
        false,
        instance,
        font,
    )?;
    add_combo_items(motion_combo, &MOTION_LABELS);

    create_checkbox(
        hwnd,
        "缓慢推近（Ken Burns）",
        form_x,
        s(224),
        s(250),
        s(24),
        ID_KEN_BURNS,
        instance,
        font,
    )?;

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

    let hint = create_static(
        hwnd,
        HINT_TEXT,
        preview_x,
        preview_y + preview_h + s(8),
        preview_w,
        s(38),
        instance,
        font,
    )?;

    // 日志视图和预览共用右侧区域；默认隐藏，点“查看日志”时切换。
    let log_height = preview_h + s(46);
    let log_view = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_CHILD
                | WS_VSCROLL
                | WS_TABSTOP
                | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                    (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32,
                ),
            preview_x,
            preview_y,
            preview_w,
            log_height,
            hwnd,
            HMENU(ID_LOG_VIEW as usize as *mut c_void),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建日志视图失败: {err}"))?;
    set_font(log_view, font);

    let button_y = s(344);
    let button_h = s(30);
    let button_w = s(90);
    let gap = s(12);
    let right = s(780) - s(16);
    create_button(
        hwnd,
        "恢复默认",
        form_x,
        button_y,
        s(120),
        button_h,
        ID_DEFAULTS,
        instance,
        font,
    )?;
    let log_button = create_button(
        hwnd,
        "查看日志",
        form_x + s(128),
        button_y,
        s(100),
        button_h,
        ID_LOG_BUTTON,
        instance,
        font,
    )?;
    create_button(
        hwnd,
        "取消",
        right - button_w * 2 - gap,
        button_y,
        button_w,
        button_h,
        ID_CANCEL,
        instance,
        font,
    )?;
    create_button(
        hwnd,
        "保存",
        right - button_w,
        button_y,
        button_w,
        button_h,
        ID_SAVE,
        instance,
        font,
    )?;

    let (settings, load_error) = match config::load_display_settings(path.as_deref()) {
        Ok(settings) => (settings, None),
        Err(err) => (DisplaySettings::default(), Some(err)),
    };
    populate_controls(hwnd, &settings);
    update_enabled(hwnd);
    let save = get_control(hwnd, ID_SAVE)?;
    if load_error.is_some() || path.is_none() {
        unsafe {
            let _ = EnableWindow(save, false);
        }
    }
    if path.is_none() && load_error.is_none() {
        set_window_text(hint, "没有可写的配置文件位置，预览可用但无法保存。");
    }

    let images = load_preview_images(&cache);
    let preview_state = PreviewState::new(
        settings,
        images,
        preview_w.max(1) as u32,
        preview_h.max(1) as u32,
    );

    Ok(SettingsState {
        hwnd,
        preview_hwnd: preview,
        log_view,
        log_button,
        hint,
        path,
        font,
        font_owned,
        load_error,
        log_mode: false,
        last_log_refresh: Instant::now(),
        last_log_len: 0,
        preview: preview_state,
    })
}

fn set_window_text(control: HWND, text: &str) {
    let wide = wide(text);
    unsafe {
        let _ = SetWindowTextW(control, PCWSTR(wide.as_ptr()));
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_font(control: HWND, font: HFONT) {
    unsafe {
        SendMessageW(control, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

fn get_control(hwnd: HWND, id: i32) -> Result<HWND, String> {
    unsafe { GetDlgItem(hwnd, id) }.map_err(|err| format!("找不到控件 {id}: {err}"))
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
            HMENU::default(),
            instance,
            None,
        )
    }
    .map_err(|err| format!("创建标签失败: {err}"))?;
    set_font(control, font);
    Ok(control)
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

fn add_combo_items(combo: HWND, items: &[&str]) {
    for item in items {
        let wide = wide(item);
        unsafe {
            SendMessageW(
                combo,
                CB_ADDSTRING,
                WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
            );
        }
    }
}

fn populate_controls(hwnd: HWND, settings: &DisplaySettings) {
    set_combo_index(hwnd, ID_LAYOUT, layout_index(settings.display_layout));
    set_combo_index(hwnd, ID_TRANSITION, transition_index(settings.transition));
    set_combo_index(hwnd, ID_MOTION, motion_index(settings.motion_mode));
    set_control_text(
        hwnd,
        ID_TRANSITION_SECONDS,
        &format_seconds(settings.transition_seconds),
    );
    set_control_text(
        hwnd,
        ID_DISPLAY_SECONDS,
        &format_seconds(settings.display_seconds),
    );
    set_checked(hwnd, ID_KEN_BURNS, settings.ken_burns);
}

fn update_enabled(hwnd: HWND) {
    let layout = layout_from(combo_index(hwnd, ID_LAYOUT));
    let motion = motion_from(combo_index(hwnd, ID_MOTION));
    let enabled = layout == DisplayLayout::Single && motion == MotionMode::Animated;
    if let Ok(control) = get_control(hwnd, ID_KEN_BURNS) {
        unsafe {
            let _ = EnableWindow(control, enabled);
        }
    }
}

fn set_combo_index(hwnd: HWND, id: i32, index: usize) {
    if let Ok(control) = get_control(hwnd, id) {
        unsafe {
            SendMessageW(control, CB_SETCURSEL, WPARAM(index), LPARAM(0));
        }
    }
}

fn set_control_text(hwnd: HWND, id: i32, text: &str) {
    if let Ok(control) = get_control(hwnd, id) {
        set_window_text(control, text);
    }
}

fn set_checked(hwnd: HWND, id: i32, checked: bool) {
    if let Ok(control) = get_control(hwnd, id) {
        unsafe {
            SendMessageW(
                control,
                BM_SETCHECK,
                WPARAM(if checked { BST_CHECKED_VALUE } else { 0 }),
                LPARAM(0),
            );
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

fn control_text(hwnd: HWND, id: i32) -> String {
    let Ok(control) = get_control(hwnd, id) else {
        return String::new();
    };
    let combo_text = window_text(control);
    // 可编辑 ComboBox 的真实文本在 EDIT 子窗口里。直接读 ComboBox 在
    // 程序化设置文本时不一定更新，读 EDIT 对键盘输入和外部设置都更可靠。
    let edit = unsafe {
        FindWindowExW(
            control,
            HWND::default(),
            w!("EDIT"),
            PCWSTR(std::ptr::null()),
        )
    };
    let mut edit_found = false;
    let mut edit_text = String::new();
    if let Ok(edit) = edit {
        if !edit.0.is_null() {
            edit_found = true;
            edit_text = window_text(edit);
        }
    }
    if std::env::var_os("PPS_DEBUG").is_some() {
        log_info!("设置读取 {id}: combo='{combo_text}' edit='{edit_text}'");
    }
    if edit_found {
        edit_text
    } else {
        combo_text
    }
}

fn window_text(control: HWND) -> String {
    let mut buffer = [0u16; 64];
    let len = unsafe { GetWindowTextW(control, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

fn checked(hwnd: HWND, id: i32) -> bool {
    let Ok(control) = get_control(hwnd, id) else {
        return false;
    };
    unsafe {
        SendMessageW(control, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == BST_CHECKED_VALUE as isize
    }
}

fn layout_index(layout: DisplayLayout) -> usize {
    match layout {
        DisplayLayout::Single => 0,
        DisplayLayout::Wall => 1,
    }
}

fn layout_from(index: Option<usize>) -> DisplayLayout {
    match index {
        Some(1) => DisplayLayout::Wall,
        _ => DisplayLayout::Single,
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

fn motion_index(motion: MotionMode) -> usize {
    match motion {
        MotionMode::Animated => 0,
        MotionMode::Static => 1,
    }
}

fn motion_from(index: Option<usize>) -> MotionMode {
    match index {
        Some(1) => MotionMode::Static,
        _ => MotionMode::Animated,
    }
}

fn format_seconds(value: f64) -> String {
    let text = format!("{value:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() {
        "0".to_string()
    } else {
        text.to_string()
    }
}

fn parse_seconds(text: &str) -> Option<f64> {
    let trimmed = text.trim().trim_end_matches('秒').trim();
    trimmed.replace(',', ".").parse::<f64>().ok()
}

impl SettingsState {
    fn read_controls(&self) -> Result<DisplaySettings, String> {
        let transition_seconds = parse_seconds(&control_text(self.hwnd, ID_TRANSITION_SECONDS))
            .ok_or_else(|| "切换时长必须是数字（0 或 0.1–3.0 秒）".to_string())?;
        let display_seconds = parse_seconds(&control_text(self.hwnd, ID_DISPLAY_SECONDS))
            .ok_or_else(|| "每张停留时间必须是数字（0.1–3600 秒）".to_string())?;
        Ok(DisplaySettings {
            display_layout: layout_from(combo_index(self.hwnd, ID_LAYOUT)),
            transition: transition_from(combo_index(self.hwnd, ID_TRANSITION)),
            transition_seconds,
            display_seconds,
            motion_mode: motion_from(combo_index(self.hwnd, ID_MOTION)),
            ken_burns: checked(self.hwnd, ID_KEN_BURNS),
        })
    }

    fn on_option_changed(&mut self) {
        update_enabled(self.hwnd);
        if let Ok(settings) = self.read_controls() {
            self.preview.set_settings(settings);
            self.invalidate_preview();
        }
    }

    fn reset_defaults(&mut self) {
        let defaults = DisplaySettings::default();
        populate_controls(self.hwnd, &defaults);
        update_enabled(self.hwnd);
        self.preview.set_settings(defaults);
        self.invalidate_preview();
    }

    fn toggle_log(&mut self) {
        self.log_mode = !self.log_mode;
        if self.log_mode {
            self.refresh_log(true);
            self.last_log_refresh = Instant::now();
            unsafe {
                let _ = ShowWindow(self.preview_hwnd, SW_HIDE);
                let _ = ShowWindow(self.hint, SW_HIDE);
                let _ = ShowWindow(self.log_view, SW_SHOW);
            }
            set_window_text(self.log_button, "返回预览");
        } else {
            unsafe {
                let _ = ShowWindow(self.log_view, SW_HIDE);
                let _ = ShowWindow(self.preview_hwnd, SW_SHOW);
                let _ = ShowWindow(self.hint, SW_SHOW);
            }
            set_window_text(self.log_button, "查看日志");
            self.invalidate_preview();
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
        set_window_text(self.log_view, &text);
        let len = text.encode_utf16().count();
        unsafe {
            SendMessageW(self.log_view, EM_SETSEL, WPARAM(len), LPARAM(len as isize));
            SendMessageW(self.log_view, EM_SCROLLCARET, WPARAM(0), LPARAM(0));
        }
    }

    fn invalidate_preview(&self) {
        unsafe {
            let _ = InvalidateRect(self.preview_hwnd, None, FALSE);
        }
    }
}

fn read_log_file(path: &std::path::Path, limit: usize) -> String {
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

fn save_settings(hwnd: HWND) {
    let Some((path, load_error, parsed)) = with_state(|state| {
        (
            state.path.clone(),
            state.load_error.clone(),
            state.read_controls(),
        )
    }) else {
        return;
    };
    if let Some(err) = load_error {
        message_error(hwnd, &err);
        return;
    }
    let settings = match parsed {
        Ok(settings) => settings,
        Err(err) => {
            message_error(hwnd, &err);
            return;
        }
    };
    match config::save_display_settings(path.as_deref(), &settings) {
        Ok(()) => {
            log_info!(
                "显示设置已保存: {}",
                path.as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(无配置文件)".to_string())
            );
            message_info(
                hwnd,
                "显示设置已保存，下次启动屏保时生效。\n命令行参数在启动时仍会覆盖对应项。",
            );
            match unsafe { DestroyWindow(hwnd) } {
                Ok(()) => log_info!("设置窗口已关闭"),
                Err(err) => log_warn!("关闭设置窗口失败: {err}"),
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
    let caption = wide("显示设置");
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
            match id {
                ID_DEFAULTS if code == 0 => {
                    with_state_mut(|state| state.reset_defaults());
                }
                ID_LOG_BUTTON if code == 0 => {
                    with_state_mut(|state| state.toggle_log());
                }
                ID_CANCEL if code == 0 => {
                    let _ = unsafe { DestroyWindow(hwnd) };
                }
                ID_SAVE if code == 0 => save_settings(hwnd),
                ID_LAYOUT | ID_TRANSITION | ID_MOTION if code == CBN_SELCHANGE => {
                    with_state_mut(|state| state.on_option_changed());
                }
                ID_TRANSITION_SECONDS | ID_DISPLAY_SECONDS
                    if code == CBN_SELCHANGE || code == CBN_EDITCHANGE =>
                {
                    with_state_mut(|state| state.on_option_changed());
                }
                ID_KEN_BURNS if code == 0 => {
                    with_state_mut(|state| state.on_option_changed());
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == PREVIEW_TIMER {
                let preview = with_state_mut(|state| {
                    if state.log_mode {
                        if state.last_log_refresh.elapsed() >= LOG_REFRESH_INTERVAL {
                            state.last_log_refresh = Instant::now();
                            state.refresh_log(false);
                        }
                        None
                    } else {
                        state.preview.update_frame(Instant::now());
                        Some(state.preview_hwnd)
                    }
                });
                if let Some(preview) = preview.flatten() {
                    unsafe {
                        let _ = InvalidateRect(preview, None, FALSE);
                    }
                }
            }
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

impl PreviewState {
    fn new(settings: DisplaySettings, images: Vec<RgbImage>, width: u32, height: u32) -> Self {
        let mut state = Self {
            images,
            scenes: None,
            settings: settings.clone(),
            width,
            height,
            frame: vec![0; width as usize * height as usize],
            scratch: vec![0; width as usize * height as usize],
            epoch: Instant::now(),
        };
        state.scenes = Some(build_scenes(
            &state.images,
            state.settings.display_layout,
            width,
            height,
        ));
        state.update_frame(Instant::now());
        state
    }

    fn set_settings(&mut self, settings: DisplaySettings) {
        let rebuild =
            settings.display_layout != self.settings.display_layout || self.scenes.is_none();
        self.settings = settings;
        if rebuild {
            self.scenes = Some(build_scenes(
                &self.images,
                self.settings.display_layout,
                self.width,
                self.height,
            ));
        }
        self.epoch = Instant::now();
        self.update_frame(Instant::now());
    }

    fn bitmap_info(&self) -> BITMAPINFO {
        BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.width as i32,
                // 负高度表示自上而下的像素顺序，和 Vec<u32> 的排列一致。
                biHeight: -(self.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                biSizeImage: self.width * self.height * 4,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn update_frame(&mut self, now: Instant) {
        let Some(scenes) = self.scenes.as_ref() else {
            return;
        };
        let settings = &self.settings;
        let animated = settings.motion_mode == MotionMode::Animated;
        let hold = settings.display_seconds.clamp(1.0, PREVIEW_MAX_HOLD);
        let transition = if animated {
            settings
                .transition_seconds
                .clamp(0.0, MAX_TRANSITION_SECONDS)
        } else {
            0.0
        };
        let cycle = hold + transition;
        if cycle <= 0.0 {
            return;
        }
        let elapsed = now.saturating_duration_since(self.epoch).as_secs_f64();
        let cycle_index = (elapsed / cycle) as u64;
        let t = elapsed % cycle;
        let from = (cycle_index % 2) as usize;
        let to = 1 - from;
        let direction = DIRECTIONS[(cycle_index as usize) % DIRECTIONS.len()];

        let ken =
            animated && settings.ken_burns && settings.display_layout == DisplayLayout::Single;
        if t < hold || transition <= 0.0 {
            let progress = if ken {
                (t / hold).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            render_frame(&scenes[from], progress, direction, &mut self.frame);
            return;
        }

        let progress = ((t - hold) / transition).clamp(0.0, 1.0) as f32;
        let seed = cycle_index.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let kind = render::resolve_transition(settings.transition, seed, None);
        let (rect, wall) = scene_rect(&scenes[to]);
        let shift =
            render::transition_layers(kind, progress, rect, (self.width, self.height), wall);
        let old_progress = if ken { 1.0 } else { 0.0 };

        match (&scenes[from], &scenes[to], kind) {
            (Frame::Single(_), Frame::Single(to_comp), render::TransitionKind::Slide(_)) => {
                render_frame(&scenes[from], old_progress, direction, &mut self.frame);
                blend_over(&mut self.frame, &to_comp.background, shift.bg_alpha);
                render::draw_poster_overlay(
                    to_comp,
                    [0.0, 0.0, 1.0, 1.0],
                    shift.offset,
                    &mut self.frame,
                );
            }
            (Frame::Wall(_), Frame::Wall(to_wall), render::TransitionKind::Slide(_)) => {
                render_frame(&scenes[from], 0.0, direction, &mut self.frame);
                blit_canvas(
                    &to_wall.canvas,
                    self.width,
                    self.height,
                    shift.offset,
                    &mut self.frame,
                );
            }
            _ => {
                render_frame(&scenes[from], old_progress, direction, &mut self.frame);
                render_frame(&scenes[to], 0.0, direction, &mut self.scratch);
                blend_over(&mut self.frame, &self.scratch, shift.bg_alpha);
            }
        }
    }
}

fn render_frame(scene: &Frame, progress: f32, direction: (f32, f32), out: &mut [u32]) {
    render::draw_frame(scene, progress, direction, out, true);
}

fn scene_rect(scene: &Frame) -> ([f32; 4], bool) {
    match scene {
        Frame::Single(comp) => (
            [
                comp.position.0 as f32,
                comp.position.1 as f32,
                comp.poster.width() as f32,
                comp.poster.height() as f32,
            ],
            false,
        ),
        Frame::Wall(wall) => ([0.0, 0.0, wall.width as f32, wall.height as f32], true),
    }
}

fn build_scenes(images: &[RgbImage], layout: DisplayLayout, width: u32, height: u32) -> [Frame; 2] {
    if images.is_empty() {
        let fallback = placeholder(0);
        return [
            Frame::Single(render::compose_single(fallback.clone(), width, height)),
            Frame::Single(render::compose_single(fallback, width, height)),
        ];
    }
    match layout {
        DisplayLayout::Single => {
            let first = images[0].clone();
            let second = images.get(1).cloned().unwrap_or_else(|| placeholder(1));
            [
                Frame::Single(render::compose_single(first, width, height)),
                Frame::Single(render::compose_single(second, width, height)),
            ]
        }
        DisplayLayout::Wall => {
            let count = images.len().clamp(2, WALL_POSTERS);
            let first: Vec<RgbImage> = (0..count)
                .map(|index| images[index % images.len()].clone())
                .collect();
            let second: Vec<RgbImage> = (0..count)
                .map(|index| images[(index + count / 2) % images.len()].clone())
                .collect();
            [
                compose_wall_or_single(first, width, height),
                compose_wall_or_single(second, width, height),
            ]
        }
    }
}

fn compose_wall_or_single(images: Vec<RgbImage>, width: u32, height: u32) -> Frame {
    render::compose_wall_frame(images, width, height)
        .unwrap_or_else(|_| Frame::Single(render::compose_single(placeholder(0), width, height)))
}

fn load_preview_images(cache: &Cache) -> Vec<RgbImage> {
    let mut cache = cache.clone();
    let mut images = Vec::new();
    for item in cache.offline_items(&[]) {
        if images.len() >= 12 {
            break;
        }
        let Some(path) = cache.get(&item) else {
            continue;
        };
        let Ok(image) = render::decode_file(&path) else {
            continue;
        };
        images.push(downscale(image, 360));
    }
    while images.len() < 2 {
        images.push(placeholder(images.len()));
    }
    images
}

fn downscale(image: RgbImage, limit: u32) -> RgbImage {
    let (width, height) = image.dimensions();
    let longest = width.max(height);
    if longest <= limit {
        return image;
    }
    let scale = limit as f32 / longest as f32;
    image::imageops::resize(
        &image,
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
        FilterType::Triangle,
    )
}

fn placeholder(index: usize) -> RgbImage {
    const COLORS: [[u8; 3]; 6] = [
        [34, 45, 74],
        [92, 48, 42],
        [44, 86, 56],
        [92, 78, 36],
        [64, 44, 92],
        [34, 82, 86],
    ];
    RgbImage::from_pixel(300, 450, image::Rgb(COLORS[index % COLORS.len()]))
}

/// `dst = dst * (1 - alpha) + src * alpha`，用于淡化和背景混合。
fn blend_over(dst: &mut [u32], src: &[u32], alpha: f32) {
    let weight = (alpha.clamp(0.0, 1.0) * 256.0).round() as u32;
    if weight == 0 {
        return;
    }
    if weight >= 256 {
        let len = dst.len().min(src.len());
        dst[..len].copy_from_slice(&src[..len]);
        return;
    }
    let inverse = 256 - weight;
    for (dst, src) in dst.iter_mut().zip(src.iter()) {
        let dr = (*dst >> 16) & 0xff;
        let dg = (*dst >> 8) & 0xff;
        let db = *dst & 0xff;
        let sr = (*src >> 16) & 0xff;
        let sg = (*src >> 8) & 0xff;
        let sb = *src & 0xff;
        let r = (dr * inverse + sr * weight + 128) >> 8;
        let g = (dg * inverse + sg * weight + 128) >> 8;
        let b = (db * inverse + sb * weight + 128) >> 8;
        *dst = (r << 16) | (g << 8) | b;
    }
}

/// 把整张画布按屏幕像素偏移覆盖到目标上，偏移外保留旧画面。
fn blit_canvas(canvas: &[u32], width: u32, height: u32, offset: (f32, f32), out: &mut [u32]) {
    let (w, h) = (width as i32, height as i32);
    let (ox, oy) = (offset.0.round() as i32, offset.1.round() as i32);
    if ox == 0 && oy == 0 {
        let len = out.len().min(canvas.len());
        out[..len].copy_from_slice(&canvas[..len]);
        return;
    }
    for sy in 0..h {
        let dy = sy + oy;
        if dy < 0 || dy >= h {
            continue;
        }
        let x0 = (-ox).max(0);
        let x1 = (w - ox).min(w);
        if x1 <= x0 {
            continue;
        }
        let len = (x1 - x0) as usize;
        let src_start = sy as usize * width as usize + x0 as usize;
        let dst_start = dy as usize * width as usize + (x0 + ox) as usize;
        if src_start + len <= canvas.len() && dst_start + len <= out.len() {
            out[dst_start..dst_start + len].copy_from_slice(&canvas[src_start..src_start + len]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(width, height, image::Rgb(rgb))
    }

    fn packed(rgb: [u8; 3]) -> u32 {
        ((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32
    }

    #[test]
    fn blend_over_hits_endpoints_and_midpoint() {
        let mut dst = vec![packed([0, 0, 0]); 4];
        let src = vec![packed([255, 100, 50]); 4];
        blend_over(&mut dst, &src, 0.0);
        assert!(dst.iter().all(|value| *value == 0));
        blend_over(&mut dst, &src, 1.0);
        assert!(dst.iter().all(|value| *value == packed([255, 100, 50])));

        // 从黑色向目标色混合一半。
        let mut halfway = vec![0u32; 4];
        blend_over(&mut halfway, &src, 0.5);
        let value = halfway[0];
        assert!((((value >> 16) & 0xff) as i32 - 128).abs() <= 1);
        assert!((((value >> 8) & 0xff) as i32 - 50).abs() <= 1);
        assert!((((value & 0xff) as i32) - 25).abs() <= 1);
    }

    #[test]
    fn blit_canvas_clips_offsets_and_keeps_old_pixels() {
        let width = 6u32;
        let height = 2u32;
        let canvas: Vec<u32> = (0..width * height).map(|index| index + 1).collect();
        let mut out = vec![99u32; (width * height) as usize];

        blit_canvas(&canvas, width, height, (0.0, 0.0), &mut out);
        assert_eq!(out, canvas);

        let mut shifted = vec![99u32; (width * height) as usize];
        blit_canvas(&canvas, width, height, (2.0, 0.0), &mut shifted);
        assert_eq!(shifted[0], 99);
        assert_eq!(shifted[1], 99);
        assert_eq!(shifted[2], 1);
        assert_eq!(shifted[3], 2);

        let mut off = vec![99u32; (width * height) as usize];
        blit_canvas(&canvas, width, height, (width as f32, 0.0), &mut off);
        assert!(off.iter().all(|value| *value == 99));
    }

    #[test]
    fn build_scenes_matches_layout() {
        let images: Vec<RgbImage> = (0..6)
            .map(|index| solid(40, 60, [(index * 30) as u8, 80, 120]))
            .collect();
        let [first, second] = build_scenes(&images, DisplayLayout::Single, 320, 180);
        assert!(matches!(first, Frame::Single(_)));
        assert!(matches!(second, Frame::Single(_)));

        let [first, second] = build_scenes(&images, DisplayLayout::Wall, 320, 180);
        match (&first, &second) {
            (Frame::Wall(a), Frame::Wall(b)) => {
                assert_eq!(a.cards, 6);
                assert_eq!(b.cards, 6);
            }
            _ => panic!("6 张应组成海报墙"),
        }
    }

    #[test]
    fn static_preview_does_not_change_within_hold() {
        let images = vec![solid(40, 60, [200, 40, 40]), solid(40, 60, [40, 200, 40])];
        let settings = DisplaySettings {
            display_layout: DisplayLayout::Single,
            transition: Transition::SlideLeft,
            transition_seconds: 1.0,
            display_seconds: 3.0,
            motion_mode: MotionMode::Static,
            ken_burns: true,
        };
        let mut preview = PreviewState::new(settings, images, 160, 90);
        preview.epoch = Instant::now();
        let epoch = preview.epoch;
        preview.update_frame(epoch);
        let first = preview.frame.clone();
        preview.update_frame(epoch + std::time::Duration::from_millis(900));
        assert_eq!(preview.frame, first, "静止模式停留段内不应有逐帧变化");
    }

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
        // 不用 %TEMP%：受限环境可能拒绝写入；放在 target 下。
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
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

        let missing = read_log_file(&dir.join("missing.log"), 2);
        assert!(missing.contains("无法读取日志"));

        let _ = fs::remove_dir_all(&dir);
    }
}

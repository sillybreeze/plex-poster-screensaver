//! Windows 托盘、会话锁屏和电源事件。
//!
//! 所有事情都在一个隐藏窗口线程里处理：托盘图标/菜单、WTS 会话通知、
//! `WM_POWERBROADCAST`。事件通过 winit 的 `EventLoopProxy` 送回主线程。

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    RegisterPowerSettingNotification, UnregisterPowerSettingNotification, HPOWERNOTIFY,
    POWERBROADCAST_SETTING,
};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, QUNS_BUSY, QUNS_PRESENTATION_MODE,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetSystemMetrics, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, KillTimer,
    LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer,
    TrackPopupMenu, TranslateMessage, GWL_STYLE, HMENU, IDI_APPLICATION, MENU_ITEM_FLAGS,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL,
    TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TRACK_POPUP_MENU_FLAGS, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL,
    WM_POWERBROADCAST, WM_RBUTTONUP, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_CAPTION,
    WS_MAXIMIZE, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::logging::{log_info, log_warn};
use crate::status;

const TRAY_ID: u32 = 1;
const WM_TRAY: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;

const MENU_PLAY: usize = 10;
const MENU_PAUSE: usize = 11;
const MENU_RESUME: usize = 12;
const MENU_CONFIG: usize = 20;
const MENU_CACHE: usize = 21;
const MENU_LOGS: usize = 22;
const MENU_QUIT: usize = 30;

struct Context {
    proxy: EventLoopProxy<UserEvent>,
    status: status::Shared,
    config_path: Option<PathBuf>,
    cache_dir: PathBuf,
    log_dir: PathBuf,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
    icon_added: bool,
    power_notify: Option<HPOWERNOTIFY>,
    display_off: bool,
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

/// 托盘线程句柄。主线程退出时发 WM_CLOSE，让托盘线程清理图标和通知。
pub struct Platform {
    hwnd: usize,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Platform {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(
                HWND(self.hwnd as *mut c_void),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start(
    proxy: EventLoopProxy<UserEvent>,
    status: status::Shared,
    config_path: Option<PathBuf>,
    cache_dir: PathBuf,
    log_dir: PathBuf,
) -> Result<Platform, String> {
    let (tx, rx) = sync_channel::<Result<usize, String>>(1);
    let thread = thread::Builder::new()
        .name("tray".to_string())
        .spawn(
            move || match create_window(proxy, status, config_path, cache_dir, log_dir) {
                Ok((hwnd, context)) => {
                    CONTEXT.with(|cell| *cell.borrow_mut() = Some(context));
                    let _ = tx.send(Ok(hwnd.0 as usize));
                    message_loop();
                }
                Err(err) => {
                    let _ = tx.send(Err(err));
                }
            },
        )
        .map_err(|err| format!("创建托盘线程失败: {err}"))?;

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(hwnd)) => Ok(Platform {
            hwnd,
            thread: Some(thread),
        }),
        Ok(Err(err)) => Err(err),
        Err(RecvTimeoutError::Timeout) => Err("托盘初始化超时".to_string()),
        Err(RecvTimeoutError::Disconnected) => Err("托盘线程提前退出".to_string()),
    }
}

fn create_window(
    proxy: EventLoopProxy<UserEvent>,
    status: status::Shared,
    config_path: Option<PathBuf>,
    cache_dir: PathBuf,
    log_dir: PathBuf,
) -> Result<(HWND, Context), String> {
    unsafe {
        let module = GetModuleHandleW(None).map_err(|err| format!("获取模块句柄失败: {err}"))?;
        let instance = HINSTANCE(module.0);
        let class_name = w!("PlexPosterScreensaverTray");

        let icon =
            LoadIconW(None, IDI_APPLICATION).map_err(|err| format!("加载托盘图标失败: {err}"))?;

        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hIcon: icon,
            lpszClassName: class_name,
            ..Default::default()
        };
        let atom = RegisterClassW(&window_class);
        if atom == 0 {
            return Err("注册托盘窗口类失败".to_string());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("Plex Poster Screensaver"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        )
        .map_err(|err| format!("创建托盘窗口失败: {err}"))?;

        let mut context = Context {
            proxy,
            status,
            config_path,
            cache_dir,
            log_dir,
            icon,
            icon_added: false,
            power_notify: None,
            display_off: false,
        };

        if let Err(err) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
            log_warn!("锁屏通知注册失败: {err}");
        }

        match RegisterPowerSettingNotification(
            windows::Win32::Foundation::HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_WINDOW_HANDLE,
        ) {
            Ok(notify) => context.power_notify = Some(notify),
            Err(err) => log_warn!("显示器电源通知注册失败: {err}"),
        }

        // Explorer 开机时可能还没准备好，加不上就靠定时器重试。
        if !add_icon(hwnd, &context) {
            log_warn!("托盘图标添加失败，稍后重试");
        }
        let _ = SetTimer(hwnd, TIMER_ID, 5_000, None);

        Ok((hwnd, context))
    }
}

fn message_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            let event = (lparam.0 as u32) & 0xffff;
            match event {
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                WM_LBUTTONDBLCLK => send_event(UserEvent::StartNow),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            handle_menu(wparam.0 & 0xffff);
            LRESULT(0)
        }
        WM_TIMER => {
            ensure_icon(hwnd);
            update_tooltip(hwnd);
            LRESULT(0)
        }
        WM_WTSSESSION_CHANGE => {
            match wparam.0 as u32 {
                WTS_SESSION_LOCK => {
                    log_info!("系统进入锁屏");
                    send_event(UserEvent::SessionLocked);
                }
                WTS_SESSION_UNLOCK => {
                    log_info!("系统解除锁屏");
                    send_event(UserEvent::SessionUnlocked);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            match wparam.0 as u32 {
                PBT_APMSUSPEND => {
                    log_info!("系统即将休眠");
                    send_event(UserEvent::PowerSuspend);
                }
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                    log_info!("系统从休眠恢复");
                    send_event(UserEvent::PowerResume);
                }
                PBT_POWERSETTINGCHANGE => {
                    if lparam.0 != 0 {
                        let setting = lparam.0 as *const POWERBROADCAST_SETTING;
                        if !setting.is_null() {
                            let setting = &*setting;
                            if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE {
                                let state = if setting.DataLength > 0 {
                                    setting.Data[0]
                                } else {
                                    1
                                };
                                let _ = with_context(|context| match state {
                                    0 if !context.display_off => {
                                        context.display_off = true;
                                        log_info!("显示器关闭，暂停渲染");
                                        let _ = context.proxy.send_event(UserEvent::DisplayOff);
                                    }
                                    1 | 2 if context.display_off => {
                                        context.display_off = false;
                                        log_info!("显示器恢复");
                                        let _ = context.proxy.send_event(UserEvent::DisplayOn);
                                    }
                                    _ => {}
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
            LRESULT(1)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            cleanup(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn cleanup(hwnd: HWND) {
    let _ = KillTimer(hwnd, TIMER_ID);
    let _ = WTSUnRegisterSessionNotification(hwnd);

    let mut context = None;
    CONTEXT.with(|cell| {
        context = cell.borrow_mut().take();
    });
    if let Some(mut context) = context {
        if let Some(notify) = context.power_notify.take() {
            let _ = UnregisterPowerSettingNotification(notify);
        }
        if context.icon_added {
            remove_icon(hwnd);
            context.icon_added = false;
        }
    }
}

fn with_context<T>(f: impl FnOnce(&mut Context) -> T) -> Option<T> {
    CONTEXT.with(|cell| {
        let mut borrow = cell.borrow_mut();
        borrow.as_mut().map(f)
    })
}

fn send_event(event: UserEvent) {
    let _ = with_context(|context| {
        if let Err(err) = context.proxy.send_event(event) {
            log_warn!("发送界面事件失败: {err}");
        }
    });
}

fn ensure_icon(hwnd: HWND) {
    let _ = with_context(|context| {
        if !context.icon_added {
            context.icon_added = add_icon(hwnd, context);
        }
    });
}

fn add_icon(hwnd: HWND, context: &Context) -> bool {
    unsafe {
        // 分两步加：先建回调，再补图标/提示。某些 shell 状态下一次性 NIM_ADD
        // 会返回 ERROR_ACCESS_DENIED，分步后至少能先把图标挂上。
        let mut data = base_icon_data(hwnd);
        data.uFlags = NIF_MESSAGE;
        data.uCallbackMessage = WM_TRAY;
        if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
            log_warn!("托盘图标添加失败，错误码 {}", GetLastError().0);
            return false;
        }

        data.uFlags = NIF_ICON;
        data.hIcon = context.icon;
        if !Shell_NotifyIconW(NIM_MODIFY, &data).as_bool() {
            log_warn!("托盘图标设置失败，错误码 {}", GetLastError().0);
        }

        data.uFlags = NIF_TIP;
        copy_wide(&mut data.szTip, &tooltip_text(context));
        if !Shell_NotifyIconW(NIM_MODIFY, &data).as_bool() {
            log_warn!("托盘提示设置失败，错误码 {}", GetLastError().0);
        }
        true
    }
}

fn remove_icon(hwnd: HWND) {
    unsafe {
        let data = base_icon_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

fn update_tooltip(hwnd: HWND) {
    let _ = with_context(|context| {
        if !context.icon_added {
            return;
        }
        unsafe {
            let mut data = base_icon_data(hwnd);
            data.uFlags = NIF_TIP;
            copy_wide(&mut data.szTip, &tooltip_text(context));
            if !Shell_NotifyIconW(NIM_MODIFY, &data).as_bool() {
                // Explorer 重启后图标会丢失，标记为未添加，下一次定时器重加。
                context.icon_added = false;
            }
        }
    });
}

fn base_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    }
}

fn tooltip_text(context: &Context) -> String {
    let status = status::snapshot(&context.status);
    let (count, _) = cache_stats(&context.cache_dir);
    let mut text = format!(
        "Plex 海报屏保 | {} | 缓存 {} 张",
        if status.online { "在线" } else { "离线" },
        count
    );
    if status.paused {
        text.push_str(" | 暂停中");
    }
    if !status.current.is_empty() {
        text.push_str(" | ");
        text.push_str(&truncate(&status.current, 40));
    }
    text
}

fn show_menu(hwnd: HWND) {
    let _ = with_context(|context| {
        let status = status::snapshot(&context.status);
        let (count, bytes) = cache_stats(&context.cache_dir);
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };

            let state = format!(
                "状态: {} | 缓存 {} 张 / {} MB",
                if status.online { "在线" } else { "离线" },
                count,
                bytes / (1024 * 1024)
            );
            append(menu, MF_STRING | MF_GRAYED, 0, &state);
            if !status.current.is_empty() {
                append(
                    menu,
                    MF_STRING | MF_GRAYED,
                    0,
                    &format!("当前: {}", truncate(&status.current, 48)),
                );
            }
            if status.paused {
                let reason = if status.pause_reason.is_empty() {
                    "暂停中".to_string()
                } else {
                    format!("暂停: {}", status.pause_reason)
                };
                append(menu, MF_STRING | MF_GRAYED, 0, &reason);
            }
            if !status.last_error.is_empty() {
                append(
                    menu,
                    MF_STRING | MF_GRAYED,
                    0,
                    &format!("最近错误: {}", truncate(&status.last_error, 60)),
                );
            }

            append(menu, MF_SEPARATOR, 0, "");
            append(menu, MF_STRING, MENU_PLAY, "立即播放");
            append(menu, MF_STRING, MENU_PAUSE, "暂停自动触发 1 小时");
            let resume = if status.paused {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            append(menu, resume, MENU_RESUME, "恢复自动触发");
            append(menu, MF_SEPARATOR, 0, "");
            let config = if context.config_path.is_some() {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            append(menu, config, MENU_CONFIG, "打开配置文件");
            append(menu, MF_STRING, MENU_CACHE, "打开缓存目录");
            append(menu, MF_STRING, MENU_LOGS, "打开日志目录");
            append(menu, MF_SEPARATOR, 0, "");
            append(menu, MF_STRING, MENU_QUIT, "退出");

            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            let _ = SetForegroundWindow(hwnd);
            let flags = TRACK_POPUP_MENU_FLAGS(TPM_RIGHTBUTTON.0 | TPM_BOTTOMALIGN.0);
            let _ = TrackPopupMenu(menu, flags, point.x, point.y, 0, hwnd, None);
            let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
    });
}

fn append(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    flags: MENU_ITEM_FLAGS,
    id: usize,
    text: &str,
) {
    let wide = to_wide(text);
    unsafe {
        let _ = AppendMenuW(menu, flags, id, PCWSTR(wide.as_ptr()));
    }
}

fn handle_menu(id: usize) {
    match id {
        MENU_PLAY => send_event(UserEvent::StartNow),
        MENU_PAUSE => send_event(UserEvent::PauseFor(Duration::from_secs(3600))),
        MENU_RESUME => send_event(UserEvent::ResumeAuto),
        MENU_QUIT => send_event(UserEvent::Quit),
        MENU_CONFIG | MENU_CACHE | MENU_LOGS => {
            let _ = with_context(|context| {
                let path = match id {
                    MENU_CONFIG => context.config_path.clone(),
                    MENU_CACHE => Some(context.cache_dir.clone()),
                    MENU_LOGS => Some(context.log_dir.clone()),
                    _ => None,
                };
                if let Some(path) = path {
                    open_path(&path);
                }
            });
        }
        _ => {}
    }
}

fn open_path(path: &Path) {
    if !path.exists() {
        log_warn!("要打开的路径不存在: {}", path.display());
        return;
    }
    let wide = to_wide(&path.to_string_lossy());
    unsafe {
        let result = ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if result.0 as isize <= 32 {
            log_warn!("打开路径失败: {}", path.display());
        }
    }
}

fn cache_stats(dir: &Path) -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut count = 0usize;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jpg") {
            continue;
        }
        count += 1;
        bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
    }
    (count, bytes)
}

/// 全屏游戏、演示模式或全屏视频时返回一个可诊断的原因。
pub fn busy_reason() -> Option<&'static str> {
    unsafe {
        if let Ok(state) = SHQueryUserNotificationState() {
            if state == QUNS_RUNNING_D3D_FULL_SCREEN {
                return Some("D3D fullscreen");
            }
            if state == QUNS_PRESENTATION_MODE {
                return Some("presentation mode");
            }
            // QUNS_BUSY 在本程序隐藏窗口存在时也可能误报；必须同时有真正的前台全屏窗口。
            if state == QUNS_BUSY && foreground_is_fullscreen() {
                return Some("shell busy + fullscreen");
            }
        }
    }
    if foreground_is_fullscreen() {
        Some("foreground fullscreen")
    } else {
        None
    }
}

fn foreground_is_fullscreen() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return false;
        }

        // 桌面、任务栏、开始菜单不算“全屏应用”。
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class);
        if len > 0 {
            let name = String::from_utf16_lossy(&class[..len as usize]);
            if matches!(
                name.as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "Windows.UI.Core.CoreWindow"
            ) {
                return false;
            }
        }

        // 有标题栏的最大化窗口即使把任务栏设为自动隐藏也排除；
        // 无标题栏的边界全屏游戏则保留。
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if style & WS_MAXIMIZE.0 != 0 && style & WS_CAPTION.0 != 0 {
            return false;
        }

        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }

        // 按窗口所在显示器比较；多屏时副屏全屏也能识别。
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let bounds = info.rcMonitor;
            return rect.left <= bounds.left
                && rect.top <= bounds.top
                && rect.right >= bounds.right
                && rect.bottom >= bounds.bottom;
        }

        let width = GetSystemMetrics(SM_CXSCREEN);
        let height = GetSystemMetrics(SM_CYSCREEN);
        rect.left <= 0 && rect.top <= 0 && rect.right >= width && rect.bottom >= height
    }
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn copy_wide(destination: &mut [u16], text: &str) {
    destination.fill(0);
    let limit = destination.len().saturating_sub(1);
    for (slot, unit) in destination.iter_mut().take(limit).zip(text.encode_utf16()) {
        *slot = unit;
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

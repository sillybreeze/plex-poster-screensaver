//! 单实例信令：已经在跑的时候再启动一次，就通知那个实例「立刻开始」。
//!
//! 用的是一个会话内命名的内核事件（`Local\` 前缀），进程退出时句柄自动关闭，
//! 事件也就跟着消失，不需要额外清理。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

use crate::config::Hotkey;

const EVENT_NAME: &str = "Local\\PlexPosterScreensaver.StartNow";

fn name() -> Vec<u16> {
    EVENT_NAME.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 已有实例在跑？有的话点亮它的信号灯并返回 true，调用方该退出了。
pub fn signal_existing() -> bool {
    let name = name();
    unsafe {
        let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) else {
            return false;
        };
        let _ = SetEvent(event);
        let _ = CloseHandle(event);
        true
    }
}

/// 创建本进程的信号灯，句柄要活到进程结束。
pub fn create() -> Option<HANDLE> {
    let name = name();
    unsafe { CreateEventW(None, false, false, PCWSTR(name.as_ptr())).ok() }
}

/// 后台等信号，每来一次就回调一次。
pub fn listen(event: HANDLE, on_trigger: impl Fn() + Send + 'static) {
    let handle = Sendable(event);
    std::thread::spawn(move || {
        while handle.wait() {
            on_trigger();
        }
    });
}

/// 注册全局热键，按下就回调一次。
///
/// `RegisterHotKey(None, ..)` 注册的是「线程级」热键，`WM_HOTKEY` 会投递到注册它的那个
/// 线程的消息队列，所以这里必须开一个自己的线程来跑消息循环 —— 主线程的窗口消息泵
/// 是 winit 的，取到没有窗口的热键消息只会丢掉。
pub fn listen_hotkey(hotkey: Hotkey, on_trigger: impl Fn() + Send + 'static) {
    std::thread::spawn(move || unsafe {
        const ID: i32 = 0x5053;
        let mods = HOT_KEY_MODIFIERS(hotkey.mods.0 | MOD_NOREPEAT.0);
        if RegisterHotKey(None, ID, mods, hotkey.vk).is_err() {
            eprintln!(
                "热键 {} 注册失败（多半被别的程序占用了），换个组合：--hotkey Ctrl+Alt+其他键",
                hotkey.label()
            );
            return;
        }
        eprintln!("热键 {} 已就绪", hotkey.label());

        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 <= 0 {
                break;
            }
            if msg.message == WM_HOTKEY && msg.wParam.0 == ID as usize {
                on_trigger();
            }
        }
        let _ = UnregisterHotKey(None, ID);
    });
}

/// 内核对象句柄是进程级的，跨线程用没问题；windows crate 只是没给它实现 Send。
struct Sendable(HANDLE);

unsafe impl Send for Sendable {}

impl Sendable {
    /// 阻塞到下一次信号；返回 false 说明句柄失效，线程该退了。
    fn wait(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, INFINITE) == WAIT_OBJECT_0 }
    }
}

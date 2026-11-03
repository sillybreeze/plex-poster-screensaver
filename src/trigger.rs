//! 单实例信令：已经在跑的时候再启动一次，就通知那个实例「立刻开始」。
//!
//! 用的是一个会话内命名的内核事件（`Local\` 前缀），进程退出时句柄自动关闭，
//! 事件也就跟着消失，不需要额外清理。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};

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

/// 内核对象句柄是进程级的，跨线程用没问题；windows crate 只是没给它实现 Send。
struct Sendable(HANDLE);

unsafe impl Send for Sendable {}

impl Sendable {
    /// 阻塞到下一次信号；返回 false 说明句柄失效，线程该退了。
    fn wait(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, INFINITE) == WAIT_OBJECT_0 }
    }
}

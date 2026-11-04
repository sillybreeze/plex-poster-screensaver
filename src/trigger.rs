//! 单实例判定与启动信令。
//!
//! 用会话内命名的互斥量原子地确定唯一运行实例：创建成功且不返回
//! `ERROR_ALREADY_EXISTS` 的进程才有运行资格。后来者只通知已有实例「立刻开始」
//! 然后退出。通知事件在竞争互斥量之前创建，这样首个实例还在初始化时也能收到信号。

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
    WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

use crate::config::Hotkey;
use crate::logging::{log_info, log_warn};

const BASE_NAME: &str = "PlexPosterScreensaver";
#[cfg(test)]
static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// 一次启动的判定结果。
pub enum Startup {
    /// 取得运行资格；句柄要活到进程结束。
    First(Guard),
    /// 已有实例在跑；`notified` 表示启动信号是否成功送达。
    Duplicate { notified: bool },
}

/// 运行资格句柄：持有命名互斥量，进程退出或本对象销毁时自动释放。
pub struct Guard {
    mutex: HANDLE,
    event: Option<HANDLE>,
}

impl Guard {
    /// 供后台监听线程使用的通知事件；创建失败时为 None。
    pub fn event(&self) -> Option<HANDLE> {
        self.event
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(event) = self.event.take() {
            unsafe {
                let _ = CloseHandle(event);
            }
        }
        unsafe {
            let _ = CloseHandle(self.mutex);
        }
    }
}

struct Names {
    event: Vec<u16>,
    mutex: Vec<u16>,
}

impl Names {
    fn default_names() -> Self {
        Self::from_base(BASE_NAME)
    }

    fn from_base(base: &str) -> Self {
        Self {
            event: wide(&format!("Local\\{base}.StartNow")),
            mutex: wide(&format!("Local\\{base}.Instance")),
        }
    }

    #[cfg(test)]
    fn isolated(tag: &str) -> Self {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self::from_base(&format!(
            "PPS.Test.{tag}.{}.{seq}.{stamp}",
            std::process::id()
        ))
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 判定本进程是否为唯一运行实例；需要通知已有实例时顺带发送启动信号。
pub fn acquire() -> Result<Startup, String> {
    acquire_with(&Names::default_names())
}

fn acquire_with(names: &Names) -> Result<Startup, String> {
    // 先创建通知事件：即使首个实例还在初始化，后来者也能把信号留在事件里。
    let event = unsafe { CreateEventW(None, false, false, PCWSTR(names.event.as_ptr())) }.ok();
    if event.is_none() {
        log_warn!(
            "创建单实例通知事件失败: {}；仍会继续做单实例判定，但后来者无法通知本实例",
            std::io::Error::last_os_error()
        );
    }

    // CreateEventW 可能留下 ERROR_ALREADY_EXISTS；先清零，避免把“事件已存在”
    // 误判成“互斥量已存在”。CreateMutexW 新建/已存在都会用 GetLastError 区分。
    unsafe {
        SetLastError(ERROR_SUCCESS);
    }
    let mutex = unsafe { CreateMutexW(None, false, PCWSTR(names.mutex.as_ptr())) }
        .map_err(|err| format!("创建单实例互斥量失败: {err}"))?;

    // CreateMutexW 成功时用 GetLastError 区分“新建”和“已存在”，这是原子判定。
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_running {
        let notified = match event {
            Some(event) => {
                let signaled = unsafe { SetEvent(event) }.is_ok();
                unsafe {
                    let _ = CloseHandle(event);
                }
                signaled
            }
            None => false,
        };
        unsafe {
            let _ = CloseHandle(mutex);
        }
        return Ok(Startup::Duplicate { notified });
    }

    Ok(Startup::First(Guard { mutex, event }))
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
            log_warn!(
                "热键 {} 注册失败（多半被别的程序占用了），换个组合：--hotkey Ctrl+Alt+其他键",
                hotkey.label()
            );
            return;
        }
        log_info!("热键 {} 已就绪", hotkey.label());

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_is_duplicate_and_signals_first() {
        let names = Names::isolated("duplicate");
        let first = match acquire_with(&names).unwrap() {
            Startup::First(guard) => guard,
            Startup::Duplicate { .. } => panic!("首个实例应取得运行资格"),
        };
        let event = first.event().expect("通知事件应创建成功");

        match acquire_with(&names).unwrap() {
            Startup::Duplicate { notified } => assert!(notified, "后来者应通知成功"),
            Startup::First(_) => panic!("第二个实例不应取得运行资格"),
        }

        // 后来者已经点了信号灯；事件是自动重置的，消费一次即回到未触发。
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);

        // 首个实例释放后，新进程应能重新取得资格（模拟异常退出后的恢复）。
        drop(first);
        match acquire_with(&names).unwrap() {
            Startup::First(_) => {}
            Startup::Duplicate { .. } => panic!("互斥量释放后应能重新取得运行资格"),
        }
    }

    #[test]
    fn leftover_event_does_not_fake_duplicate() {
        let names = Names::isolated("event-only");
        // 只有事件、没有互斥量（上一个实例的事件句柄残留或其它场景），
        // CreateEventW 会留下 ERROR_ALREADY_EXISTS；不能因此误判为已有实例。
        let leftover =
            unsafe { CreateEventW(None, false, false, PCWSTR(names.event.as_ptr())) }.unwrap();
        match acquire_with(&names).unwrap() {
            Startup::First(_) => {}
            Startup::Duplicate { .. } => panic!("只有通知事件时不应判定为已有实例"),
        }
        unsafe {
            let _ = CloseHandle(leftover);
        }
    }
}

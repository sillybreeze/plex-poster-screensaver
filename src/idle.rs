//! 空闲检测：判断「人还在不在」。
//!
//! 两种口径：
//! - 默认走 Windows 的 `GetLastInputInfo`，和系统屏保同一个标准 —— 键盘、鼠标都算活动。
//! - `mouse_only` 改成轮询鼠标指针位置和按键状态，完全不看键盘。

use std::time::{Duration, Instant};

/// 系统级「最后一次输入」的时间戳。
///
/// 单位是 `GetTickCount` 的毫秒，32 位，大约 49.7 天回绕一次，所以只能做差值比较。
///
/// 失败（安全桌面、非交互会话等）时返回 `None`。调用方必须把「不知道」当成
/// 暂停判断，而不是当成「刚有输入」或「已经空闲很久」——返回 0 会被算成开机时长。
#[cfg(windows)]
pub fn last_input_tick() -> Option<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut info).as_bool() } {
        Some(info.dwTime)
    } else {
        None
    }
}

#[cfg(not(windows))]
pub fn last_input_tick() -> Option<u32> {
    None
}

/// 距离最后一次输入过了多久；检测失败时返回 `None`。
#[cfg(windows)]
pub fn idle_from_last_input() -> Option<Duration> {
    use windows::Win32::System::SystemInformation::GetTickCount;

    let last = last_input_tick()?;
    let now = unsafe { GetTickCount() };
    Some(tick_elapsed(now, last))
}

#[cfg(not(windows))]
pub fn idle_from_last_input() -> Option<Duration> {
    None
}

/// `GetTickCount` 是 32 位毫秒，49.7 天回绕一次，差值必须用 wrapping 运算。
#[cfg(windows)]
fn tick_elapsed(now: u32, last: u32) -> Duration {
    Duration::from_millis(now.wrapping_sub(last) as u64)
}

/// 鼠标指针位置 + 三个按键的按下状态（只看鼠标，不看键盘）。
#[cfg(windows)]
fn mouse_state() -> ((i32, i32), u8) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT::default();
    let pos = match unsafe { GetCursorPos(&mut point) } {
        Ok(()) => (point.x, point.y),
        Err(_) => (0, 0),
    };

    let mut buttons = 0u8;
    for (bit, key) in [(1u8, VK_LBUTTON), (2, VK_RBUTTON), (4, VK_MBUTTON)] {
        // 高位 = 当前是否按下。低位是「上次调用以来按过」，跨线程不可靠，不用。
        if (unsafe { GetAsyncKeyState(key.0 as i32) } as u16) & 0x8000 != 0 {
            buttons |= bit;
        }
    }
    (pos, buttons)
}

#[cfg(not(windows))]
fn mouse_state() -> ((i32, i32), u8) {
    ((0, 0), 0)
}

/// 空闲计时器。`poll` 由事件循环每 500ms 调一次。
pub struct Idle {
    mouse_only: bool,
    last_activity: Instant,
    cursor: (i32, i32),
    buttons: u8,
    primed: bool,
}

impl Idle {
    pub fn new(mouse_only: bool, now: Instant) -> Self {
        Self {
            mouse_only,
            last_activity: now,
            cursor: (0, 0),
            buttons: 0,
            primed: false,
        }
    }

    /// 屏保关掉之后重新计时，避免刚退出就又被判成空闲。
    pub fn reset(&mut self, now: Instant) {
        self.last_activity = now;
        self.primed = false;
    }

    /// 轮询一次，返回当前已经空闲了多久；检测不可用时返回 `None`。
    pub fn poll(&mut self, now: Instant) -> Option<Duration> {
        if !self.mouse_only {
            return idle_from_last_input();
        }

        let (cursor, buttons) = mouse_state();
        if self.primed && (cursor != self.cursor || buttons != self.buttons) {
            self.last_activity = now;
        }
        self.cursor = cursor;
        self.buttons = buttons;
        self.primed = true;
        Some(now.saturating_duration_since(self.last_activity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn tick_delta_wraps_around() {
        // 回绕边界：now 比 last 小，差值仍然是正确的一小段时间。
        assert_eq!(tick_elapsed(100, u32::MAX - 50), Duration::from_millis(151));
        assert_eq!(tick_elapsed(1000, 1000), Duration::ZERO);
    }
}

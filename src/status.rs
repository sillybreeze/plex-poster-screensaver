//! 托盘/日志共享的运行状态。工作线程和界面线程都可能更新它。

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub online: bool,
    pub current: String,
    pub last_error: String,
    pub paused: bool,
    pub pause_reason: String,
}

pub type Shared = Arc<Mutex<Status>>;

pub fn new() -> Shared {
    Arc::new(Mutex::new(Status::default()))
}

pub fn snapshot(shared: &Shared) -> Status {
    let status = shared.lock().unwrap_or_else(|poison| poison.into_inner());
    status.clone()
}

pub fn update<T>(shared: &Shared, f: impl FnOnce(&mut Status) -> T) -> T {
    let mut status = shared.lock().unwrap_or_else(|poison| poison.into_inner());
    f(&mut status)
}

//! 简单文件日志：后台/开机自启时没有控制台，错误要能落盘。
//!
//! 日志写到 `%LOCALAPPDATA%\PlexPosterScreensaver\logs\app.log`，启动时若超过
//! 5MB 轮换成 `app.1.log`。没有初始化成功时仍用 `eprintln!`，不影响程序运行。

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

struct Logger {
    file: Option<File>,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

pub fn log_dir() -> PathBuf {
    crate::config::app_dir().join("logs")
}

/// 初始化文件日志；重复调用无副作用。
pub fn init() {
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("app.log");

    if fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(&path, dir.join("app.1.log"));
    }

    let file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Some(file),
        Err(err) => {
            eprintln!("日志文件不可写 {}: {err}", path.display());
            None
        }
    };
    let _ = LOGGER.set(Mutex::new(Logger { file }));
}

pub fn log(level: Level, args: fmt::Arguments<'_>) {
    let message = args.to_string();
    // 有控制台时保持原来的可见性；没有控制台/终端被关掉时不要把程序 panic 掉。
    let _ = writeln!(std::io::stderr(), "{message}");

    let Some(logger) = LOGGER.get() else {
        return;
    };
    let Ok(mut logger) = logger.lock() else {
        return;
    };
    let Some(file) = logger.file.as_mut() else {
        return;
    };

    let line = format!("[{}] {:<5} {}\n", timestamp(), level.as_str(), message);
    let _ = file.write_all(line.as_bytes());
    let _ = file.flush();
}

fn timestamp() -> String {
    let time = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}

macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Info, format_args!($($arg)*))
    };
}

macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Warn, format_args!($($arg)*))
    };
}

macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Error, format_args!($($arg)*))
    };
}

pub(crate) use {log_error, log_info, log_warn};

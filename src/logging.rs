//! 简单文件日志：后台/开机自启时没有控制台，错误要能落盘。
//!
//! 日志写到 `%LOCALAPPDATA%\PlexPosterScreensaver\logs\app.log`，运行期间超过
//! 5MB 时轮换成 `app.1.log`。只保留 `app.log` 和 `app.1.log` 两个文件；没有
//! 初始化成功时仍用 `eprintln!`，不影响程序运行。

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const LOG_NAME: &str = "app.log";
const BACKUP_NAME: &str = "app.1.log";

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
    path: PathBuf,
    backup: PathBuf,
    written: u64,
    max_bytes: u64,
}

impl Logger {
    fn open(dir: &Path) -> Self {
        Self::open_with_limit(dir, MAX_LOG_BYTES)
    }

    fn open_with_limit(dir: &Path, max_bytes: u64) -> Self {
        let _ = fs::create_dir_all(dir);
        let path = dir.join(LOG_NAME);
        let backup = dir.join(BACKUP_NAME);

        // 启动时先把超限日志轮换掉，避免本次运行继续往超大文件追加。
        if fs::metadata(&path)
            .map(|meta| meta.len() > max_bytes)
            .unwrap_or(false)
        {
            rotate_files(&path, &backup);
        }

        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                Self {
                    file: Some(file),
                    path,
                    backup,
                    written,
                    max_bytes,
                }
            }
            Err(err) => {
                eprintln!("日志文件不可写 {}: {err}", path.display());
                Self {
                    file: None,
                    path,
                    backup,
                    written: 0,
                    max_bytes,
                }
            }
        }
    }

    fn write_line(&mut self, line: &str) {
        if self.file.is_none() {
            return;
        }
        if self.written > 0 && self.written.saturating_add(line.len() as u64) > self.max_bytes {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.write_all(line.as_bytes()).is_ok() && file.flush().is_ok() {
            self.written = self.written.saturating_add(line.len() as u64);
        } else {
            // 写失败（磁盘满、文件被删等）就停止尝试，避免每次日志都重复报错。
            self.file = None;
        }
    }

    fn rotate(&mut self) {
        self.file = None;
        rotate_files(&self.path, &self.backup);
        self.written = 0;
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.file = Some(file);
        }
    }
}

/// 只保留一份备份：先删旧的 `app.1.log`，再把当前文件改名过去。
///
/// Windows 的 rename 不覆盖已存在的目标，所以删除步骤不能省；删除失败时至少
/// 把当前文件移除，避免继续写超大日志。
fn rotate_files(path: &Path, backup: &Path) {
    let _ = fs::remove_file(backup);
    if fs::rename(path, backup).is_err() {
        let _ = fs::remove_file(path);
    }
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

pub fn log_dir() -> PathBuf {
    crate::config::app_dir().join("logs")
}

/// 初始化文件日志；重复调用无副作用。
pub fn init() {
    let logger = Logger::open(&log_dir());
    let _ = LOGGER.set(Mutex::new(logger));
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

    let line = format!("[{}] {:<5} {}\n", timestamp(), level.as_str(), message);
    logger.write_line(&line);
}

/// 日志限流：首次立即输出，之后每个窗口最多汇总一次。
///
/// 用于持续故障场景，避免同一错误按帧率刷日志。`record` 返回 `Some(n)` 表示
/// 现在应当输出，`n` 是上次输出以来被抑制的次数（首次为 0）。
#[derive(Debug, Default)]
pub struct Throttle {
    last: Option<Instant>,
    suppressed: u32,
}

impl Throttle {
    pub fn reset(&mut self) {
        self.last = None;
        self.suppressed = 0;
    }

    pub fn record(&mut self, now: Instant, interval: Duration) -> Option<u32> {
        let due = self
            .last
            .map(|last| now.saturating_duration_since(last) >= interval)
            .unwrap_or(true);
        if due {
            self.last = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // 放 target 下而不是系统临时目录：部分受限环境不允许写 %TEMP%。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("log-tests")
            .join(format!("{tag}-{}-{stamp}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotates_while_running_and_keeps_one_backup() {
        let dir = temp_dir("rotate");
        let mut logger = Logger::open_with_limit(&dir, 64);

        logger.write_line(&format!("{}\n", "a".repeat(40)));
        logger.write_line(&format!("{}\n", "b".repeat(40)));
        let current = fs::read_to_string(dir.join(LOG_NAME)).unwrap();
        let backup = fs::read_to_string(dir.join(BACKUP_NAME)).unwrap();
        assert!(current.contains('b'));
        assert!(backup.contains('a'));

        // 再写一轮：旧备份被替换，不会无限累积文件。
        logger.write_line(&format!("{}\n", "c".repeat(40)));
        let current = fs::read_to_string(dir.join(LOG_NAME)).unwrap();
        let backup = fs::read_to_string(dir.join(BACKUP_NAME)).unwrap();
        assert!(current.contains('c'));
        assert!(backup.contains('b'));
        assert!(!backup.contains('a'));
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            2,
            "只应保留 app.log 和 app.1.log"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn throttle_reports_suppressed_count() {
        let now = Instant::now();
        let interval = Duration::from_secs(30);
        let mut throttle = Throttle::default();

        assert_eq!(throttle.record(now, interval), Some(0));
        assert_eq!(throttle.record(now, interval), None);
        assert_eq!(
            throttle.record(now + Duration::from_secs(1), interval),
            None
        );
        assert_eq!(
            throttle.record(now + interval, interval),
            Some(2),
            "窗口到期时应汇总期间被抑制的条数"
        );

        throttle.reset();
        assert_eq!(
            throttle.record(now + interval, interval),
            Some(0),
            "reset 后重新从首次输出开始"
        );
    }
}

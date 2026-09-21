//! # 文件日志
//!
//! 日志统一写入 `~/.auto-shutdown/logs/`，按天分割、自动清理：
//!
//! - **主程序日志** `app-YYYY-MM-DD.log`：心跳、扫描、倒计时、关机等全部事件；
//! - **ESP8266 上报日志** `esp-YYYY-MM-DD.log`：设备随心跳连接推送回来的运行
//!   日志，由本程序收到后落盘（ESP 自身不写文件）。
//!
//! 清理策略：启动时删除修改时间早于 [`RETENTION_DAYS`] 的 `*.log` 文件；
//! 跨天切换文件时顺带再清一次。
//!
//! 多线程安全：心跳监控线程与 UI 线程都会写日志，通过全局 `Mutex` 串行化。

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use chrono::Local;

/// 日志保留天数（超过自动删除）
const RETENTION_DAYS: u64 = 30;

/// 单个日志文件的写入状态（app / esp 各一份）
struct LogFile {
    /// 文件名前缀（日志类型），如 "app" / "esp"
    prefix: &'static str,
    file: Option<std::fs::File>,
    /// 当前文件对应的日期（YYYY-MM-DD），跨天时切换文件
    date: String,
}

impl LogFile {
    const fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            file: None,
            date: String::new(),
        }
    }
}

static APP_LOG: LazyLock<Mutex<LogFile>> = LazyLock::new(|| Mutex::new(LogFile::new("app")));
static ESP_LOG: LazyLock<Mutex<LogFile>> = LazyLock::new(|| Mutex::new(LogFile::new("esp")));

/// 日志目录：`~/.auto-shutdown/logs`
pub fn logs_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".auto-shutdown").join("logs")
}

/// 程序启动时调用：建目录、清理过期日志
pub fn init() {
    let dir = logs_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[logger] 无法创建日志目录 {}: {e}", dir.display());
        return;
    }
    let removed = cleanup(&dir);
    if removed > 0 {
        // 文件在首次写入时才打开，这里先打印；随后主流程的启动日志会落盘
        eprintln!("[logger] 已清理 {removed} 个过期日志文件");
    }
}

/// 写一条主程序日志（时间戳与级别由本函数统一加）
pub fn write(level: &str, msg: &str) {
    write_to(&APP_LOG, level, msg);
}

/// 写一条 ESP8266 上报的日志（以 PC 收到时刻为准）
pub fn write_esp(msg: &str) {
    write_to(&ESP_LOG, "ESP", msg);
}

fn write_to(cell: &LazyLock<Mutex<LogFile>>, level: &str, msg: &str) {
    // 锁被毒化（写日志时 panic）也要继续工作
    let Ok(mut log) = cell.lock() else { return };
    let now = Local::now();
    let today = now.format("%Y-%m-%d").to_string();

    // 首次写入或跨天：打开今天的新文件，并顺带清理过期日志
    if log.date != today {
        let dir = logs_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}-{today}.log", log.prefix));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                log.file = Some(f);
                log.date = today;
                cleanup(&dir);
            }
            Err(e) => {
                eprintln!("[logger] 打开日志文件 {} 失败: {e}", path.display());
                return;
            }
        }
    }

    let line = format!("{} [{level}] {msg}\n", now.format("%Y-%m-%d %H:%M:%S%.3f"));
    if let Some(f) = log.file.as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
    // debug 构建同时打到控制台，便于开发期观察（release 为纯 GUI 程序无控制台）
    #[cfg(debug_assertions)]
    eprint!("{line}");
}

/// 删除目录下修改时间早于保留期的 `*.log` 文件，返回删除数量
fn cleanup(dir: &Path) -> usize {
    let cutoff = SystemTime::now() - Duration::from_secs(RETENTION_DAYS * 24 * 3600);
    let mut removed = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "log") {
                continue;
            }
            let expired = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if expired && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// 写一条 INFO 日志：`log_info!("心跳正常 rtt={}ms", rtt)`
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logger::write("INFO", &format!($($arg)*)) };
}

/// 写一条 WARN 日志
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logger::write("WARN", &format!($($arg)*)) };
}

/// 写一条 ERROR 日志
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::logger::write("ERROR", &format!($($arg)*)) };
}

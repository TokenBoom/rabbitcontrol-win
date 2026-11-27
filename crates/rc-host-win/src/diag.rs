//! 诊断日志：后台进程没有控制台，把关键事件追加到配置目录下的 `agent.log`。
//!
//! 启动时发现日志超过 1 MiB 就重新开始，避免无限增长。日志只写本地事件与
//! 错误原因，不写令牌、密码或证书内容。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 日志文件名（位于配置根目录）。
const LOG_FILE: &str = "agent.log";
/// 启动时发现日志超过这个大小就截断重来。
const ROTATE_BYTES: u64 = 1024 * 1024;

static SINK: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// 日志文件路径（与主界面的配置同目录）。
pub fn path() -> PathBuf {
    rc_client_core::app_prefs::default_prefs_path().with_file_name(LOG_FILE)
}

/// 打开日志文件；失败时后续写入静默丢弃（不能因为写不了日志而起不来）。
pub fn init(file: &Path) {
    if std::fs::metadata(file)
        .map(|m| m.len() > ROTATE_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(file);
    }
    let sink = OpenOptions::new().create(true).append(true).open(file).ok();
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = sink;
}

/// 写一行日志（自动带本地时间前缀）。
pub fn write(line: &str) {
    let mut guard = SINK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(file, "{} {line}", timestamp());
    }
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, min, sec) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // 粗略的日历换算足以诊断用（1970 起）
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}Z")
}

/// 天数 → 年月日（Howard Hinnant 的算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// 带格式化的日志写入。
#[macro_export]
macro_rules! diag_log {
    ($($arg:tt)*) => {
        $crate::diag::write(&format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(18_993), (2022, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        assert_eq!(civil_from_days(20_819), (2027, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29), "闰日");
    }
}

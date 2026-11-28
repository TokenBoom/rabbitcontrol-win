//! 内部事件日志（`tracing`）开关：设了环境变量 `RC_TRACE` 才生效。
//!
//! core 的采集 / 编码 / 协议 / 会话都用 `tracing` 打点，但没人装订阅器时这些事件全部丢弃，
//! 出了问题只能靠猜。诊断时设置环境变量即可看到内部事件，例如：
//!
//! ```text
//! set RC_TRACE=info                         # 全部 info 级别
//! set RC_TRACE=rc_client_core=debug,rc_session=debug
//! ```
//!
//! 不设环境变量时什么都不做（`tracing` 在没有订阅器时几乎没有开销）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use tracing_subscriber::EnvFilter;

/// 默认过滤表达式（环境变量设了但为空时使用）。
const DEFAULT_DIRECTIVES: &str = "info";

/// 按环境变量 `env_var` 的过滤表达式，把内部事件写到 `make_writer` 每次返回的写入端。
///
/// 返回是否启用了日志（环境变量没设，或已经装过订阅器时为 `false`）。
pub fn init_with<W, F>(env_var: &str, make_writer: F) -> bool
where
    W: Write + 'static,
    F: Fn() -> W + Send + Sync + 'static,
{
    let Ok(raw) = std::env::var(env_var) else {
        return false;
    };
    let raw = raw.trim();
    let filter = EnvFilter::try_new(if raw.is_empty() {
        DEFAULT_DIRECTIVES
    } else {
        raw
    })
    .unwrap_or_else(|_| EnvFilter::new(DEFAULT_DIRECTIVES));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_writer(make_writer)
        .try_init()
        .is_ok()
}

/// 写到标准错误（有控制台的程序用）。
pub fn init_to_stderr(env_var: &str) -> bool {
    init_with(env_var, std::io::stderr)
}

/// 追加写到文件（没有控制台的程序用，例如后台被控进程）。
pub fn init_to_file(env_var: &str, path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
        return false;
    };
    init_with(env_var, move || match file.try_clone() {
        Ok(clone) => Box::new(clone) as Box<dyn Write + Send>,
        Err(_) => Box::new(std::io::sink()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_variable_leaves_logging_off() {
        // 测试进程里不应该有人打开过内部日志；这个用例的变量名专用于测试，确保没设
        const NEVER_SET: &str = "RC_TRACE_TEST_VARIABLE_THAT_IS_NOT_SET";
        assert!(std::env::var_os(NEVER_SET).is_none());
        assert!(!init_to_stderr(NEVER_SET));
    }
}

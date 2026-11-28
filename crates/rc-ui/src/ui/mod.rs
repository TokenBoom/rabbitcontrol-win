//! UI 壳层：窗口宿主、XAML 资产与可复用构件。

pub mod brand;
pub mod dialogs;
pub mod session_window;
pub mod shell;
pub mod widgets;

pub use shell::{Shell, ShellNotifier, DESIGN_HEIGHT, DESIGN_WIDTH};

use std::sync::OnceLock;

/// 调试：带 `--no-activate` 启动时窗口只显示、不抢前台焦点（仅调试构建）。
/// 给自动化截图用：用户在用这台电脑时，窗口弹出来不会把键盘焦点夺走。
pub fn no_activate() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        cfg!(debug_assertions) && std::env::args().skip(1).any(|arg| arg == "--no-activate")
    })
}

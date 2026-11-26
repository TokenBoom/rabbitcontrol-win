//! UI 壳层：窗口宿主、XAML 资产与可复用构件。

pub mod shell;
pub mod widgets;

pub use shell::{Shell, ShellNotifier, DESIGN_HEIGHT, DESIGN_WIDTH};

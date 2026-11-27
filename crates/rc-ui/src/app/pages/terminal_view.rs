//! 终端渲染：VT 屏幕模型（[`crate::biz::vt::VtScreen`]）→ `RichTextBlock`。
//!
//! 每行一个 [`Paragraph`]（固定行高），相同样式的连续文本一个 [`Run`]：
//! 前景色 / 粗体 / 斜体 / 下划线。底色不绘制（见 `vt` 模块的已知限制）。
//! 画刷按颜色缓存，避免每帧重建。
//!
//! 0.37 投影不把基类方法挂到派生类上：行高在 `Block`、文字属性在 `TextElement`，
//! 需先 cast 再调用。

use windows::core::{Interface, Result, HSTRING};
use windows::UI::Color;
use windows::UI::Text::{FontStyle, FontWeights, TextDecorations};
use windows::UI::Xaml::Controls::RichTextBlock;
use windows::UI::Xaml::Documents::{Block, Inline, Paragraph, Run, TextElement};
use windows::UI::Xaml::LineStackingStrategy;
use windows::UI::Xaml::Media::{Brush, SolidColorBrush};

use crate::biz::vt::{self, DEFAULT_BG, DEFAULT_FG};

use super::super::App;

/// 请求远端 PTY 的列数（与 `terminal_open_with_options` 一致）。
pub(crate) const TERMINAL_COLS: u16 = 120;
/// 请求远端 PTY 的行数。
pub(crate) const TERMINAL_ROWS: u16 = 30;
/// 单次渲染的行数上限（历史过长时只画最近部分，保 UI 流畅）。
const RENDER_LINES: usize = 300;
/// 终端行高（DIP）。
const LINE_HEIGHT: f64 = 18.0;

/// 清空 `block` 并写入 `lines`：每行一段，相同样式的连续文本一个 Run。
///
/// `brush_for` 取指定 `0xRRGGBB` 的画刷（调用方可缓存）。
pub(crate) fn write_terminal_lines(
    block: &RichTextBlock,
    lines: &[vt::Line],
    mut brush_for: impl FnMut(u32) -> Result<Brush>,
) -> Result<()> {
    let blocks = block.Blocks()?;
    blocks.Clear()?;
    let bold = FontWeights::SemiBold()?;
    for line in lines {
        let paragraph = Paragraph::new()?;
        let as_block: Block = paragraph.cast()?;
        as_block.SetLineHeight(LINE_HEIGHT)?;
        as_block.SetLineStackingStrategy(LineStackingStrategy::BlockLineHeight)?;
        let inlines = paragraph.Inlines()?;
        for span in line {
            let run = Run::new()?;
            run.SetText(HSTRING::from(span.text.as_str()))?;
            let style = span.style;
            let text: TextElement = run.cast()?;
            let fg = style.resolve_fg_only(DEFAULT_FG, DEFAULT_BG).to_hex();
            text.SetForeground(&brush_for(fg)?)?;
            if style.bold {
                text.SetFontWeight(bold)?;
            }
            if style.italic {
                text.SetFontStyle(FontStyle::Italic)?;
            }
            if style.underline {
                text.SetTextDecorations(TextDecorations::Underline)?;
            }
            inlines.Append(&run.cast::<Inline>()?)?;
        }
        blocks.Append(&as_block)?;
    }
    Ok(())
}

/// `0xRRGGBB` 纯色画刷。
pub(crate) fn solid_brush(hex: u32) -> Result<SolidColorBrush> {
    let brush = SolidColorBrush::new()?;
    brush.SetColor(Color {
        A: 255,
        R: (hex >> 16) as u8,
        G: (hex >> 8) as u8,
        B: hex as u8,
    })?;
    Ok(brush)
}

impl App {
    /// 把终端模型渲染到 `TerminalOutput`（脏标志驱动：每 tick 至多一次）。
    pub(crate) fn render_terminal(&mut self) -> Result<()> {
        let Some(block) = self.find_opt::<RichTextBlock>("TerminalOutput") else {
            return Ok(());
        };
        let lines = self.terminal.render_lines(RENDER_LINES);
        let cache = &mut self.terminal_brushes;
        write_terminal_lines(&block, &lines, |hex| {
            if let Some(brush) = cache.get(&hex) {
                return brush.cast();
            }
            let brush = solid_brush(hex)?;
            cache.insert(hex, brush.clone());
            brush.cast()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::UI::Xaml::Hosting::WindowsXamlManager;
    use windows::UI::Xaml::Markup::XamlReader;

    /// 在真实 XAML 运行时里加载会话窗口并渲染一段带样式的终端输出：
    /// 验证 `session_window.xaml` 可解析，以及行 / Run 的全部属性调用成立。
    #[test]
    #[ignore] // 需要 XAML Islands 运行时：cargo test -p rc-ui terminal_render -- --ignored
    fn terminal_render_smoke() {
        let hr = unsafe {
            windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_SINGLETHREADED,
            )
        };
        assert!(hr.is_ok(), "RoInitialize: {hr:?}");
        let _manager =
            WindowsXamlManager::InitializeForCurrentThread().expect("XAML Islands 初始化");
        // 会话窗口 XAML 引用 StaticResource 样式，必须先合并样式字典
        crate::app::install_styles().expect("合并样式字典");
        let root: windows::UI::Xaml::FrameworkElement =
            XamlReader::Load(include_str!("../../ui/assets/session_window.xaml"))
                .expect("session_window.xaml 解析")
                .cast()
                .expect("应为 FrameworkElement");
        let block: RichTextBlock = root
            .FindName("TerminalOutput")
            .expect("TerminalOutput")
            .cast()
            .expect("应为 RichTextBlock");

        let mut screen = vt::VtScreen::new(80, 4);
        screen
            .feed("\x1b[1;31merror:\x1b[0m file not found\r\n\x1b[4mlink\x1b[24m \x1b[3mit\x1b[0m");
        write_terminal_lines(&block, &screen.render_lines(usize::MAX), |hex| {
            solid_brush(hex)?.cast()
        })
        .expect("渲染终端输出");

        let blocks = block.Blocks().expect("Blocks");
        assert_eq!(blocks.Size().expect("段数"), 4, "4 行屏幕 = 4 段");
        let first: Paragraph = blocks.GetAt(0).expect("第 1 段").cast().expect("Paragraph");
        let inlines = first.Inlines().expect("Inlines");
        assert_eq!(
            inlines.Size().expect("Run 数"),
            2,
            "红色粗体 + 默认色两个 Run"
        );
        let second: Paragraph = blocks.GetAt(1).expect("第 2 段").cast().expect("Paragraph");
        assert_eq!(
            second.Inlines().expect("Inlines").Size().expect("Run 数"),
            3,
            "下划线 / 默认 / 斜体"
        );
    }
}

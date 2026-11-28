//! 终端渲染与输入（会话窗口里的工具面板）。
//!
//! 屏幕内容来自 [`Terminal`]（纯逻辑的视口 / 选区 / 绘制指令），这里只做三件事：
//!
//! 1. **度量**：量出等宽字体的字宽，定下格子（列宽 / 行高 / 字间距）——每个字符恰好占一格，文本才能
//!    按列对齐。全角字符靠字体回退渲染，宽度不一定正好两格，所以它们各自成段、从确定的列起头；
//! 2. **画**：两张叠放的 `Canvas`——底下铺底色条 / 选区 / 光标，上面放文本。元素按行入池复用，
//!    每次刷新只改属性、不重建；内容没变的行直接跳过；
//! 3. **输入**：透明的 `TextBox`（`TerminalIme`）持有键盘焦点，承接可打印字符与输入法；其余按键在
//!    `PreviewKeyDown` 里用 [`encode_key`] 编码——被本地消费的才标记 Handled 并经事件通道发给 App。

use std::collections::HashMap;

use windows::core::{Interface, Result, HSTRING};
use windows::Foundation::{Size, TypedEventHandler};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
use windows::UI::Input::PointerPointProperties;
use windows::UI::Text::{FontStyle, FontWeights, TextDecorations};
use windows::UI::Xaml::Controls::{Canvas, Control, Grid, Panel, TextBlock, TextBox};
use windows::UI::Xaml::Input::{KeyEventHandler, PointerEventHandler};
use windows::UI::Xaml::Media::{Brush, FontFamily, SolidColorBrush};
use windows::UI::Xaml::Shapes::{Rectangle, Shape};
use windows::UI::Xaml::{
    FocusState, FrameworkElement, LineStackingStrategy, RoutedEventHandler,
    SizeChangedEventHandler, TextWrapping, UIElement, Visibility,
};

use crate::biz::terminal::{Cursor, Frame, Op, ScrollRange, Terminal, TextStyle};
use crate::biz::terminal_keys::{encode_key, KeyAction, Modifiers};
use rc_client_core::session_bridge::SessionUpdate;

use super::super::actions::{Event, Events, TerminalEvent, TerminalPointer};
use super::super::{copy_to_clipboard, App};

/// 等宽字体：有 Cascadia Mono（Windows 11 自带）就用它，否则 Consolas（Windows 10 起都有）。
const FONT_FAMILY: &str = "Cascadia Mono, Consolas";
/// 候选字号（逻辑像素）。
const FONT_SIZES: [f64; 5] = [13.0, 14.0, 15.0, 16.0, 17.0];
/// 量字宽用的字符个数。
const RULER_CHARS: usize = 40;
/// 右侧留给滚动条滑块的宽度。
const SCROLLBAR_LANE: f64 = 10.0;
/// 连击判定：间隔（微秒）与位移（逻辑像素）。
const DOUBLE_CLICK_US: u64 = 500_000;
const DOUBLE_CLICK_SLACK: f64 = 4.0;
/// 滚轮一格滚三行（Windows 默认）。
const WHEEL_LINES_PER_NOTCH: isize = 3;
/// 面板拖动尺寸会连续触发 `Resized`：停稳这么多 tick 再把新行列数告诉远端。
const RESIZE_DEBOUNCE_TICKS: u64 = 2;
/// 滚动条滑块最短长度（逻辑像素），再短就抓不住了。
const MIN_THUMB: f64 = 24.0;
/// 滚动条滑块的宽度与它离画面右缘的间隙（逻辑像素；宽度与 XAML 里 `TerminalThumb` 的 `Width` 一致）。
const THUMB_WIDTH: f64 = 4.0;
const THUMB_GAP: f64 = 3.0;

/// 终端格子度量（逻辑像素）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Metrics {
    /// 字号。
    pub font_size: f64,
    /// 一列的宽度：取整数，格线落在整像素上，不同颜色的相邻文本之间不会出现缝。
    pub cell: f64,
    /// 一行的高度（整数）。
    pub line: f64,
    /// 字间距（1/1000 em）：把字体原本的字宽补足到整列宽。
    pub tracking: i32,
}

/// 在候选字号里挑一个：字宽离上一个整数最近（补足到整列的字间距最小）的那个。
/// `advance` 给出某字号下的自然字宽；量不出来（返回不大于 0）的字号跳过。
fn pick_metrics(mut advance: impl FnMut(f64) -> f64) -> Option<Metrics> {
    let mut best: Option<(f64, Metrics)> = None;
    for font_size in FONT_SIZES {
        let natural = advance(font_size);
        if natural <= 0.0 {
            continue;
        }
        let cell = natural.ceil();
        let leftover = (cell - natural) / font_size;
        let metrics = Metrics {
            font_size,
            cell,
            line: (font_size * 1.35).round(),
            tracking: ((cell - natural) / font_size * 1000.0).round() as i32,
        };
        // 补足不到字号的 3% 就直接用；否则留一个补得最少的
        if leftover <= 0.03 {
            return Some(metrics);
        }
        if best.map_or(true, |(ratio, _)| leftover < ratio) {
            best = Some((leftover, metrics));
        }
    }
    best.map(|(_, metrics)| metrics)
}

/// 用真实的 XAML 文本布局量字宽，定下格子度量。
pub(crate) fn measure_metrics() -> Option<Metrics> {
    let ruler = TextBlock::new().ok()?;
    ruler
        .SetFontFamily(FontFamily::CreateInstanceWithName(HSTRING::from(FONT_FAMILY)).ok()?)
        .ok()?;
    let sample = "0".repeat(RULER_CHARS);
    ruler.SetText(HSTRING::from(sample.as_str())).ok()?;
    let layout: UIElement = ruler.cast().ok()?;
    let advance = |font_size: f64| -> f64 {
        let _ = ruler.SetFontSize(font_size);
        let _ = layout.Measure(Size {
            Width: f32::INFINITY,
            Height: f32::INFINITY,
        });
        layout
            .DesiredSize()
            .map(|size| f64::from(size.Width) / RULER_CHARS as f64)
            .unwrap_or(0.0)
    };
    pick_metrics(advance)
}

/// 滚动条滑块长度：与「一屏占总行数的比例」成正比，但不短于 [`MIN_THUMB`]。
fn thumb_length(track: f64, range: ScrollRange) -> f64 {
    let total = (range.scrollable + range.page) as f64;
    (track * range.page as f64 / total).clamp(MIN_THUMB, track)
}

/// 滚动条滑块顶端离轨道顶端的距离。
fn thumb_offset(track: f64, thumb: f64, range: ScrollRange) -> f64 {
    if range.scrollable == 0 {
        return 0.0;
    }
    (track - thumb) * range.offset as f64 / range.scrollable as f64
}

/// 一个背景条。0.37 投影不把基类方法挂到派生类上，用得到的三种身份各留一份。
#[derive(Clone)]
struct RectEl {
    shape: Shape,
    layout: FrameworkElement,
    element: UIElement,
}

/// 一个文本块（同上，按用得到的身份拆开）。
#[derive(Clone)]
struct TextEl {
    text: TextBlock,
    layout: FrameworkElement,
    element: UIElement,
}

/// 一行的元素池：背景条放底层画布，文本放上层画布。
#[derive(Default)]
struct RowPool {
    rects: Vec<RectEl>,
    texts: Vec<TextEl>,
}

impl RowPool {
    /// 第 `index` 个背景条，不够就新建一个（多出来的在画布里收起，下回接着用）。
    fn rect(&mut self, index: usize, canvas: &Panel) -> Result<RectEl> {
        while self.rects.len() <= index {
            let rect = Rectangle::new()?;
            let element = RectEl {
                shape: rect.cast()?,
                layout: rect.cast()?,
                element: rect.cast()?,
            };
            canvas.Children()?.Append(&element.element)?;
            self.rects.push(element);
        }
        Ok(self.rects[index].clone())
    }

    /// 第 `index` 个文本块，不够就按格子度量新建一个。
    fn text(&mut self, index: usize, canvas: &Panel, metrics: Metrics) -> Result<TextEl> {
        while self.texts.len() <= index {
            let block = TextBlock::new()?;
            block.SetFontFamily(FontFamily::CreateInstanceWithName(HSTRING::from(
                FONT_FAMILY,
            ))?)?;
            block.SetFontSize(metrics.font_size)?;
            block.SetCharacterSpacing(metrics.tracking)?;
            // 行盒固定为格子的行高；系统「文本缩放」不能把格子撑歪
            block.SetLineHeight(metrics.line)?;
            block.SetLineStackingStrategy(LineStackingStrategy::BlockLineHeight)?;
            block.SetIsTextScaleFactorEnabled(false)?;
            block.SetIsTextSelectionEnabled(false)?;
            block.SetTextWrapping(TextWrapping::NoWrap)?;
            let element = TextEl {
                text: block.clone(),
                layout: block.cast()?,
                element: block.cast()?,
            };
            element.layout.SetHeight(metrics.line)?;
            canvas.Children()?.Append(&element.element)?;
            self.texts.push(element);
        }
        Ok(self.texts[index].clone())
    }
}

/// 终端画面上可变的部分：度量、元素池、画刷缓存与指针状态。
#[derive(Default)]
pub(crate) struct TerminalView {
    /// 格子度量；XAML 量不出来时为 `None`（终端画面不画）。
    pub metrics: Option<Metrics>,
    /// 画面的宽高（画滚动条滑块用）；画面还没排过版时为 `None`。
    track: Option<(f64, f64)>,
    rows: Vec<RowPool>,
    brushes: HashMap<u32, Brush>,
    /// 上次画的指令：与新一帧逐行比较，没变的行不动。
    last: Option<Frame>,
    /// 上次画的时候光标是什么样子。
    cursor: Option<Cursor>,
    /// 上次按左键的时间与位置（数连击），以及数到第几击。
    last_press: Option<(u64, f64, f64)>,
    clicks: u8,
    /// 正在拖选区 / 正在拖滚动条滑块（值是指针相对滑块顶端的偏移）。
    selecting: bool,
    thumb_drag: Option<f64>,
}

/// 终端锁屏校验状态（被控端锁屏时，验证系统密码后才能用终端）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TerminalLock {
    /// 被控端要求先验证。
    pub required: bool,
    /// 本会话已验证通过。
    pub unlocked: bool,
}

/// 取指定 `0xRRGGBB` 的画刷（按颜色缓存）。
fn brush(brushes: &mut HashMap<u32, Brush>, color: u32) -> Result<Brush> {
    if let Some(brush) = brushes.get(&color) {
        return Ok(brush.clone());
    }
    let brush = SolidColorBrush::CreateInstanceWithColor(windows::UI::Color {
        A: 255,
        R: (color >> 16) as u8,
        G: (color >> 8) as u8,
        B: color as u8,
    })?;
    let brush: Brush = brush.cast()?;
    brushes.insert(color, brush.clone());
    Ok(brush)
}

fn decorations(style: TextStyle) -> TextDecorations {
    let mut value = TextDecorations::None;
    if style.underline {
        value |= TextDecorations::Underline;
    }
    if style.strike {
        value |= TextDecorations::Strikethrough;
    }
    value
}

/// 画一行的指令。
fn draw_row(
    ops: &[Op],
    row: usize,
    pool: &mut RowPool,
    bg: &Panel,
    fg: &Panel,
    metrics: Metrics,
    brushes: &mut HashMap<u32, Brush>,
) -> Result<()> {
    let top = row as f64 * metrics.line;
    let (mut rects, mut texts) = (0usize, 0usize);
    for op in ops {
        match op {
            Op::Rect { col, cols, color } => {
                let rect = pool.rect(rects, bg)?;
                rect.shape.SetFill(&brush(brushes, color.to_hex())?)?;
                Canvas::SetLeft(&rect.element, *col as f64 * metrics.cell)?;
                Canvas::SetTop(&rect.element, top)?;
                rect.layout.SetWidth(*cols as f64 * metrics.cell)?;
                rect.layout.SetHeight(metrics.line)?;
                rect.element.SetVisibility(Visibility::Visible)?;
                rects += 1;
            }
            Op::Text {
                col,
                text,
                color,
                style,
            } => {
                let block = pool.text(texts, fg, metrics)?;
                block.text.SetText(HSTRING::from(text.as_str()))?;
                block.text.SetForeground(&brush(brushes, color.to_hex())?)?;
                block.text.SetFontWeight(if style.bold {
                    FontWeights::SemiBold()?
                } else {
                    FontWeights::Normal()?
                })?;
                block.text.SetFontStyle(if style.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                })?;
                block.text.SetTextDecorations(decorations(*style))?;
                Canvas::SetLeft(&block.element, *col as f64 * metrics.cell)?;
                Canvas::SetTop(&block.element, top)?;
                block.element.SetVisibility(Visibility::Visible)?;
                texts += 1;
            }
        }
    }
    for rect in pool.rects.iter().skip(rects) {
        rect.element.SetVisibility(Visibility::Collapsed)?;
    }
    for text in pool.texts.iter().skip(texts) {
        text.element.SetVisibility(Visibility::Collapsed)?;
    }
    Ok(())
}

/// 把指针位置换算成（绝对行号，列号）：落在画面下方之外算最后一行，右缘之外算行尾之后一格。
fn cell_at(terminal: &Terminal, metrics: Metrics, x: f64, y: f64) -> (usize, usize) {
    let (cols, rows) = terminal.size();
    let row = (y / metrics.line).floor().clamp(0.0, rows as f64 - 1.0) as usize;
    let col = (x / metrics.cell).floor().clamp(0.0, cols as f64) as usize;
    (terminal.viewport_top() + row, col)
}

/// 此刻按着的修饰键（事件参数在 0.37 投影里不带 Shift / Ctrl 状态，直接问系统）。
fn pressed_modifiers() -> Modifiers {
    // SAFETY：GetKeyState 只读本线程消息队列的键盘状态，任何线程都能调
    let down = |key: u16| unsafe { GetKeyState(i32::from(key)) } < 0;
    Modifiers {
        shift: down(VK_SHIFT.0),
        ctrl: down(VK_CONTROL.0),
        alt: down(VK_MENU.0),
    }
}

/// 按下的是哪个键：0 左 / 1 右 / 2 中。
fn pressed_button(properties: &PointerPointProperties) -> u8 {
    let down =
        |probe: fn(&PointerPointProperties) -> Result<bool>| probe(properties).unwrap_or(false);
    if down(PointerPointProperties::IsLeftButtonPressed) {
        0
    } else if down(PointerPointProperties::IsRightButtonPressed) {
        1
    } else if down(PointerPointProperties::IsMiddleButtonPressed) {
        2
    } else {
        0
    }
}

/// 指针事件挂在哪里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerTarget {
    /// 终端画面：选区、滚轮、右键复制 / 粘贴。
    Screen,
    /// 滚动条滑块：拖动翻历史。
    Thumb,
}

/// 给终端画面或滚动条滑块挂指针事件（按下时捕获指针，拖出画面之外也能继续收到移动与松开）。
fn wire_pointer(element: &UIElement, target: PointerTarget, events: Events) -> Result<()> {
    let pressed = element.clone();
    let sender = events.clone();
    // 坐标相对这块画面自己（GetCurrentPoint(None) 是相对整个 XAML 根的）
    let reference = element.clone();
    element.PointerPressed(PointerEventHandler::new(move |_, args| {
        if let Some(args) = args {
            let point = args.GetCurrentPoint(&reference)?;
            let position = point.Position()?;
            let (x, y) = (f64::from(position.X), f64::from(position.Y));
            let pointer = match target {
                PointerTarget::Screen => TerminalPointer::Press {
                    x,
                    y,
                    button: pressed_button(&point.Properties()?),
                    time_us: point.Timestamp()?,
                },
                PointerTarget::Thumb => TerminalPointer::Thumb { y, pressed: true },
            };
            if let Ok(id) = args.Pointer() {
                let _ = pressed.CapturePointer(&id);
            }
            args.SetHandled(true)?;
            sender.send(Event::Terminal(TerminalEvent::Pointer(pointer)));
        }
        Ok(())
    }))?;
    let sender = events.clone();
    let reference = element.clone();
    element.PointerMoved(PointerEventHandler::new(move |_, args| {
        if let Some(args) = args {
            let point = args.GetCurrentPoint(&reference)?;
            let position = point.Position()?;
            let (x, y) = (f64::from(position.X), f64::from(position.Y));
            let pointer = match target {
                // 只有按着左键拖动才算延伸选区
                PointerTarget::Screen => {
                    if point.Properties()?.IsLeftButtonPressed().unwrap_or(false) {
                        Some(TerminalPointer::Move { x, y })
                    } else {
                        None
                    }
                }
                PointerTarget::Thumb => Some(TerminalPointer::Thumb { y, pressed: false }),
            };
            if let Some(pointer) = pointer {
                sender.send(Event::Terminal(TerminalEvent::Pointer(pointer)));
            }
        }
        Ok(())
    }))?;
    let sender = events.clone();
    element.PointerReleased(PointerEventHandler::new(move |_, args| {
        if let Some(args) = args {
            args.SetHandled(true)?;
            sender.send(Event::Terminal(TerminalEvent::Pointer(
                TerminalPointer::Release,
            )));
        }
        Ok(())
    }))?;
    if target == PointerTarget::Screen {
        let sender = events;
        element.PointerWheelChanged(PointerEventHandler::new(move |_, args| {
            if let Some(args) = args {
                let delta = args
                    .GetCurrentPoint(None)?
                    .Properties()?
                    .MouseWheelDelta()?;
                args.SetHandled(true)?;
                sender.send(Event::Terminal(TerminalEvent::Pointer(
                    TerminalPointer::Wheel { delta },
                )));
            }
            Ok(())
        }))?;
    }
    Ok(())
}

/// 异步读剪贴板文本，结果经事件通道回传（UI 线程不能阻塞等待 WinRT 的异步操作）。
fn read_clipboard_text(events: Events) {
    use windows::ApplicationModel::DataTransfer::Clipboard;
    use windows::Foundation::AsyncOperationCompletedHandler;

    let outcome = Clipboard::GetContent().and_then(|content| content.GetTextAsync());
    match outcome {
        Ok(operation) => {
            let _ =
                operation.SetCompleted(AsyncOperationCompletedHandler::new(move |operation, _| {
                    let result = operation.as_ref().map_or_else(
                        || Err("读取剪贴板失败".to_string()),
                        |operation| {
                            operation
                                .get()
                                .map(|text: HSTRING| text.to_string())
                                .map_err(|err| err.to_string())
                        },
                    );
                    events.send(Event::ClipboardRead(result));
                    Ok(())
                }));
        }
        Err(err) => events.send(Event::ClipboardRead(Err(err.to_string()))),
    }
}

impl TerminalView {
    /// 下一帧全部重画（会话换了 / 画面行数变了之后）。
    pub(crate) fn invalidate(&mut self) {
        self.last = None;
    }

    /// 画面是否已经排过版（有了实际尺寸，行列数才是真实的）。
    pub(crate) fn measured(&self) -> bool {
        self.track.is_some()
    }
}

/// 调试用：`--demo-terminal`。Windows 被控端没有终端服务，本机做不了端到端的终端联调——
/// 这里不连远端，直接开会话窗口把一段有代表性的远端输出喂给终端面板，键入由本地回显。
#[cfg(debug_assertions)]
impl App {
    /// 安排演示：从下一个 tick 起（会话窗口等消息循环跑起来再开）。
    pub(crate) fn start_terminal_demo(&mut self) -> Result<()> {
        self.terminal_demo = Some(crate::biz::terminal_demo::Demo::new());
        Ok(())
    }

    /// 开演示窗口：会话窗口 + 工具面板停在终端 tab。
    fn open_terminal_demo_window(&mut self) -> Result<()> {
        self.ensure_session_window("终端演示")?;
        self.tool_panel_visible = true;
        self.tool_tab = 0;
        self.set_visible("ToolPanel", true)?;
        self.set_visible("ToolSplitter", true)?;
        self.apply_tool_width()?;
        // 演示里「终端」一直开着：输入照常走编码与回显
        self.terminal_id = Some(0);
        self.set_terminal_hint("调试演示：输出是脚本放的，键入会本地回显")?;
        self.focus_terminal()?;
        self.terminal_dirty = true;
        Ok(())
    }

    /// 每个 tick 喂一段演示输出（脚本放完后不再喂）。
    #[cfg(debug_assertions)]
    pub(crate) fn pump_terminal_demo(&mut self) -> Result<()> {
        let Some(demo) = self.terminal_demo.as_mut() else {
            return Ok(());
        };
        if self.session_host.is_none() {
            return self.open_terminal_demo_window();
        }
        if let Some(chunk) = demo.next_chunk() {
            self.terminal.feed_output(&chunk);
            self.terminal_dirty = true;
        }
        self.terminal_tick()
    }
}

impl App {
    /// 会话窗口创建后给终端面板接线（元素位于会话窗口根）。
    pub(crate) fn wire_terminal(&self) -> Result<()> {
        // 键盘：先于输入框自己的处理。被本地消费的按键标记 Handled，输入框就不再收它
        let ime: TextBox = self.find("TerminalIme")?;
        let ime_element: UIElement = ime.clone().cast()?;
        ime_element.PreviewKeyDown(KeyEventHandler::new({
            let events = self.sender();
            move |_, args| {
                if let Some(args) = args {
                    let code = args.Key()?.0 as u32;
                    let mods = pressed_modifiers();
                    let action = encode_key(code, mods, false);
                    // 修饰键本身不拦：输入框要靠它维持 Shift / Ctrl 状态（输入法也盯着）
                    if action.is_consumed() && !matches!(code, 0x10..=0x12) {
                        args.SetHandled(true)?;
                        events.send(Event::Terminal(TerminalEvent::Key {
                            code,
                            shift: mods.shift,
                            ctrl: mods.ctrl,
                            alt: mods.alt,
                        }));
                    }
                }
                Ok(())
            }
        }))?;
        ime.TextChanged(windows::UI::Xaml::Controls::TextChangedEventHandler::new({
            let events = self.sender();
            move |_, _| {
                events.send(Event::Terminal(TerminalEvent::TextCommitted));
                Ok(())
            }
        }))?;
        // 输入法组合输入的这两个事件要 Windows 10 2004 起才有；更老的系统上注册不上，
        // 那就退回「输入框里一有字就发」（拼音组字的中间状态会被当成最终文本，打字本身不受影响）
        let composition = ime
            .TextCompositionStarted(TypedEventHandler::<
                TextBox,
                windows::UI::Xaml::Controls::TextCompositionStartedEventArgs,
            >::new({
                let events = self.sender();
                move |_, _| {
                    events.send(Event::Terminal(TerminalEvent::Composition(true)));
                    Ok(())
                }
            }))
            .and_then(|_| {
                ime.TextCompositionEnded(TypedEventHandler::<
                    TextBox,
                    windows::UI::Xaml::Controls::TextCompositionEndedEventArgs,
                >::new({
                    let events = self.sender();
                    move |_, _| {
                        events.send(Event::Terminal(TerminalEvent::Composition(false)));
                        Ok(())
                    }
                }))
            });
        if composition.is_err() {
            eprintln!("[ui] 终端：输入法组合事件不可用（旧系统），输入按逐字上屏处理");
        }
        ime_element.GotFocus(RoutedEventHandler::new({
            let events = self.sender();
            move |_, _| {
                events.send(Event::Terminal(TerminalEvent::Focus(true)));
                Ok(())
            }
        }))?;
        ime_element.LostFocus(RoutedEventHandler::new({
            let events = self.sender();
            move |_, _| {
                events.send(Event::Terminal(TerminalEvent::Focus(false)));
                Ok(())
            }
        }))?;
        // 指针与尺寸
        let host: UIElement = self.find::<Grid>("TerminalHost")?.cast()?;
        wire_pointer(&host, PointerTarget::Screen, self.sender())?;
        let host_layout: FrameworkElement = host.cast()?;
        host_layout.SizeChanged(SizeChangedEventHandler::new({
            let events = self.sender();
            move |_, _| {
                events.send(Event::Terminal(TerminalEvent::Resized));
                Ok(())
            }
        }))?;
        let thumb: UIElement = self.find::<Rectangle>("TerminalThumb")?.cast()?;
        wire_pointer(&thumb, PointerTarget::Thumb, self.sender())
    }

    /// 终端面板事件（UI 线程）。
    pub(crate) fn on_terminal_event(&mut self, event: TerminalEvent) -> Result<()> {
        match event {
            TerminalEvent::Key {
                code,
                shift,
                ctrl,
                alt,
            } => self.on_terminal_key(code, Modifiers { shift, ctrl, alt }),
            TerminalEvent::TextCommitted => self.on_terminal_text(),
            TerminalEvent::Composition(active) => {
                self.terminal_composing = active;
                // 上屏：组合输入一结束，输入框里就是最终文本
                if !active {
                    self.on_terminal_text()?;
                }
                Ok(())
            }
            TerminalEvent::Focus(focused) => {
                self.terminal_focused = focused;
                self.terminal_dirty = true;
                Ok(())
            }
            TerminalEvent::Pointer(pointer) => self.on_terminal_pointer(pointer),
            TerminalEvent::Resized => self.fit_terminal_grid(),
        }
    }

    fn on_terminal_key(&mut self, code: u32, mods: Modifiers) -> Result<()> {
        if !self.terminal_input_allowed() {
            return Ok(());
        }
        self.terminal_blink.touch(self.tick);
        match encode_key(code, mods, self.terminal.screen().application_cursor_keys()) {
            KeyAction::Send(bytes) => self.send_terminal_input(&bytes),
            KeyAction::Copy => self.copy_terminal_selection(),
            KeyAction::Paste => {
                self.request_terminal_paste();
                Ok(())
            }
            KeyAction::ScrollPage(pages) => {
                self.terminal.scroll_pages(pages);
                self.terminal_dirty = true;
                Ok(())
            }
            KeyAction::Compose | KeyAction::Ignore => Ok(()),
        }
    }

    /// 输入框里的文本（可打印字符 / 输入法上屏）发走并清空。组合输入进行中不动它：
    /// 那是输入法正在拼的字，清掉会把这次输入打断。
    fn on_terminal_text(&mut self) -> Result<()> {
        if self.terminal_composing {
            return Ok(());
        }
        let Some(ime) = self.find_opt::<TextBox>("TerminalIme") else {
            return Ok(());
        };
        let text = ime.Text().unwrap_or_default().to_string();
        if text.is_empty() {
            return Ok(());
        }
        ime.SetText(HSTRING::new())?;
        if self.terminal_input_allowed() {
            self.terminal_blink.touch(self.tick);
            self.send_terminal_input(text.as_bytes())?;
        }
        Ok(())
    }

    fn on_terminal_pointer(&mut self, pointer: TerminalPointer) -> Result<()> {
        let Some(metrics) = self.terminal_view.metrics else {
            return Ok(());
        };
        match pointer {
            TerminalPointer::Press {
                x,
                y,
                button,
                time_us,
            } => match button {
                0 => {
                    // 点画面 = 想在终端里敲字：把键盘焦点交给输入框
                    self.focus_terminal()?;
                    let clicks = match self.terminal_view.last_press {
                        Some((last_us, last_x, last_y))
                            if time_us.saturating_sub(last_us) < DOUBLE_CLICK_US
                                && (x - last_x).abs() < DOUBLE_CLICK_SLACK
                                && (y - last_y).abs() < DOUBLE_CLICK_SLACK =>
                        {
                            // 单击 → 双击选词 → 三击选整行 → 再点又是单击
                            self.terminal_view.clicks % 3 + 1
                        }
                        _ => 1,
                    };
                    self.terminal_view.last_press = Some((time_us, x, y));
                    self.terminal_view.clicks = clicks;
                    self.terminal
                        .select_at(cell_at(&self.terminal, metrics, x, y), clicks);
                    self.terminal_view.selecting = true;
                }
                // 右键：有选区就复制并清掉，没有就粘贴（Windows 终端的惯例）
                1 => {
                    self.focus_terminal()?;
                    if self.terminal.has_selection() {
                        self.copy_terminal_selection()?;
                    } else {
                        self.request_terminal_paste();
                    }
                }
                _ => {}
            },
            TerminalPointer::Move { x, y } => {
                if self.terminal_view.selecting {
                    self.terminal
                        .extend_selection(cell_at(&self.terminal, metrics, x, y));
                }
            }
            TerminalPointer::Release => {
                self.terminal_view.selecting = false;
                self.terminal_view.thumb_drag = None;
            }
            TerminalPointer::Wheel { delta } => {
                // 滚轮向上滚（delta 为正）= 翻回历史
                let notches = isize::from(delta as i16) / 120;
                self.terminal.scroll_by(-notches * WHEEL_LINES_PER_NOTCH);
            }
            TerminalPointer::Thumb { y, pressed } => {
                let Some(track) = self.terminal_view.track.map(|(_, height)| height) else {
                    return Ok(());
                };
                let range = self.terminal.scroll_range();
                let thumb = thumb_length(track, range);
                if pressed {
                    // 按在滑块上：记下指针相对滑块顶端的偏移；按在轨道别处：滑块跳过来居中
                    let offset = thumb_offset(track, thumb, range);
                    self.terminal_view.thumb_drag =
                        Some(if (offset..=offset + thumb).contains(&y) {
                            y - offset
                        } else {
                            thumb / 2.0
                        });
                }
                if let Some(grab) = self.terminal_view.thumb_drag {
                    let fraction = ((y - grab) / (track - thumb).max(1.0)).clamp(0.0, 1.0);
                    self.terminal
                        .scroll_to_offset((fraction * range.scrollable as f64).round() as usize);
                }
            }
        }
        self.terminal_dirty = true;
        Ok(())
    }

    /// 终端面板此刻是否露在外面（面板打开且停在终端 tab）。
    fn terminal_shown(&self) -> bool {
        self.tool_panel_visible && self.tool_tab == 0 && self.session_host.is_some()
    }

    /// 能不能往远端终端发输入：有终端、对方允许、没被锁屏校验挡住。
    fn terminal_input_allowed(&self) -> bool {
        self.terminal_id.is_some()
            && self.terminal_allowed
            && !(self.terminal_lock.required && !self.terminal_lock.unlocked)
    }

    fn focus_terminal(&self) -> Result<()> {
        let ime: TextBox = self.find("TerminalIme")?;
        let control: Control = ime.cast()?;
        control.Focus(FocusState::Programmatic)?;
        Ok(())
    }

    /// 把字节发给远端终端。大段粘贴按数据帧分块，单帧超限会被远端拒收。
    pub(crate) fn send_terminal_input(&mut self, bytes: &[u8]) -> Result<()> {
        // 调试演示：没有远端，键入交给本地回显
        #[cfg(debug_assertions)]
        if let Some(output) = self.terminal_demo.as_mut().map(|demo| demo.echo(bytes)) {
            self.terminal.feed_output(&output);
            self.terminal_dirty = true;
            return Ok(());
        }
        let (Some(term_id), Some(active)) = (self.terminal_id, self.session.as_ref()) else {
            return Ok(());
        };
        let chunk = rc_tools::TERMINAL_DATA_CHUNK_SIZE;
        for part in bytes.chunks(chunk.max(1)) {
            if let Err(err) = active.handle.tools().terminal_input(term_id, part) {
                return self.set_terminal_hint(&format!("发送失败：{err}"));
            }
        }
        Ok(())
    }

    fn copy_terminal_selection(&mut self) -> Result<()> {
        let text = self.terminal.selected_text();
        if text.is_empty() {
            return Ok(());
        }
        copy_to_clipboard(&text);
        self.terminal.clear_selection();
        self.terminal_dirty = true;
        Ok(())
    }

    fn request_terminal_paste(&mut self) {
        if self.terminal_input_allowed() {
            read_clipboard_text(self.sender());
        }
    }

    /// 剪贴板文本读回来了（粘贴）。
    pub(crate) fn on_clipboard_read(
        &mut self,
        result: std::result::Result<String, String>,
    ) -> Result<()> {
        match result {
            Ok(text) if !text.is_empty() => {
                let bytes = self.terminal.paste_bytes(&text);
                self.send_terminal_input(&bytes)
            }
            Ok(_) => Ok(()),
            Err(err) => self.set_terminal_hint(&format!("粘贴失败：{err}")),
        }
    }

    pub(crate) fn set_terminal_hint(&self, text: &str) -> Result<()> {
        self.label("TerminalHint", text)
    }

    /// 由画面尺寸算出列数行数（面板大小、分隔条、窗口缩放都会走到这里）。
    pub(crate) fn fit_terminal_grid(&mut self) -> Result<()> {
        let (Some(metrics), Some(host)) = (
            self.terminal_view.metrics,
            self.find_opt::<FrameworkElement>("TerminalHost"),
        ) else {
            return Ok(());
        };
        let width = host.ActualWidth()?;
        let height = host.ActualHeight()?;
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }
        self.terminal_view.track = Some((width, height));
        let cols = ((width - SCROLLBAR_LANE) / metrics.cell).floor().max(1.0) as usize;
        let rows = (height / metrics.line).floor().max(1.0) as usize;
        if self.terminal.resize(cols, rows) {
            self.terminal_resize_due = Some(self.tick + RESIZE_DEBOUNCE_TICKS);
            self.terminal_dirty = true;
        }
        Ok(())
    }

    /// 每个 tick：停稳后把新行列数告诉远端；光标闪烁 / 输出变了就重画。
    pub(crate) fn terminal_tick(&mut self) -> Result<()> {
        if self.terminal_resize_due.is_some_and(|due| self.tick >= due) {
            self.terminal_resize_due = None;
            let (cols, rows) = self.terminal.size();
            let (Some(term_id), Some(active)) = (self.terminal_id, self.session.as_ref()) else {
                return Ok(());
            };
            let cols = u16::try_from(cols).unwrap_or(u16::MAX);
            let rows = u16::try_from(rows).unwrap_or(u16::MAX);
            if let Err(err) = active.handle.tools().terminal_resize(term_id, cols, rows) {
                return self.set_terminal_hint(&format!("调整终端尺寸失败：{err}"));
            }
        }
        self.render_terminal()
    }

    /// 把终端画面画到两层画布上（脏标志驱动；光标明暗变化时也重画）。
    pub(crate) fn render_terminal(&mut self) -> Result<()> {
        // 失焦时留一个暗色方块提示光标位置；闪烁只在焦点在终端上时进行
        let cursor = match (self.terminal_id.is_some(), self.terminal_focused) {
            (false, _) => Cursor::Hidden,
            (true, false) => Cursor::Idle,
            (true, true) if self.terminal_blink.visible(self.tick) => Cursor::Solid,
            (true, true) => Cursor::Hidden,
        };
        if !self.terminal_dirty && self.terminal_view.cursor == Some(cursor) {
            return Ok(());
        }
        if !self.terminal_shown() {
            // 面板收着：留到露出来再画
            return Ok(());
        }
        self.terminal_dirty = false;
        self.terminal_view.cursor = Some(cursor);
        let Some(metrics) = self.terminal_view.metrics else {
            return Ok(());
        };
        let bg: Panel = self.find("TerminalBg")?;
        let fg: Panel = self.find("TerminalFg")?;
        let frame = self.terminal.frame(cursor);
        let view = &mut self.terminal_view;
        if view.rows.len() != frame.rows.len() {
            // 行数变了（画面尺寸变了）：旧元素的行位置全不对了，清空重画
            bg.Children()?.Clear()?;
            fg.Children()?.Clear()?;
            view.rows = frame.rows.iter().map(|_| RowPool::default()).collect();
            view.last = None;
        }
        for (index, ops) in frame.rows.iter().enumerate() {
            let unchanged = view.last.as_ref().and_then(|last| last.rows.get(index)) == Some(ops);
            if !unchanged {
                draw_row(
                    ops,
                    index,
                    &mut view.rows[index],
                    &bg,
                    &fg,
                    metrics,
                    &mut view.brushes,
                )?;
            }
        }
        view.last = Some(frame);

        self.render_terminal_thumb()?;
        // 输入框跟着光标走：输入法的候选窗出现在打字的位置
        if let Some((row, col)) = self.terminal.cursor_cell() {
            let ime: TextBox = self.find("TerminalIme")?;
            let ime: UIElement = ime.cast()?;
            Canvas::SetLeft(&ime, col as f64 * metrics.cell)?;
            Canvas::SetTop(&ime, row as f64 * metrics.line)?;
        }
        self.set_visible("TerminalEmpty", self.terminal_id.is_none())?;
        self.set_visible("TerminalThumb", self.terminal.scroll_range().scrollable > 0)?;
        // 翻看历史时下面又来了新输出：给一个回到底部的入口，不然不知道错过了
        // 翻看历史时下面来了新输出：给一个回到底部的入口，不然不知道错过了
        self.set_visible("TerminalBehind", self.terminal.has_new_output())?;
        let close: Control = self.find("TerminalClose")?;
        close.SetIsEnabled(self.terminal_id.is_some())?;
        self.sync_terminal_lock()
    }

    fn render_terminal_thumb(&self) -> Result<()> {
        let (Some((width, track)), Some(thumb)) = (
            self.terminal_view.track,
            self.find_opt::<Rectangle>("TerminalThumb"),
        ) else {
            return Ok(());
        };
        let range = self.terminal.scroll_range();
        if range.scrollable == 0 {
            return Ok(());
        }
        let length = thumb_length(track, range);
        let thumb_layout: FrameworkElement = thumb.cast()?;
        let thumb_element: UIElement = thumb.cast()?;
        thumb_layout.SetHeight(length)?;
        // 滑块是 Canvas 的直接子元素：横纵坐标都要自己给（贴右缘，留一点间隙）
        Canvas::SetLeft(&thumb_element, width - THUMB_WIDTH - THUMB_GAP)?;
        Canvas::SetTop(&thumb_element, thumb_offset(track, length, range))?;
        Ok(())
    }

    /// 锁屏校验浮层的显隐：被控端锁屏且本会话还没验证通过时挡在终端前面。
    pub(crate) fn sync_terminal_lock(&self) -> Result<()> {
        let locked = self.terminal_lock.required && !self.terminal_lock.unlocked;
        let was = self
            .find_opt::<UIElement>("TerminalLock")
            .is_some_and(|gate| gate.Visibility().is_ok_and(|v| v == Visibility::Visible));
        self.set_visible("TerminalLock", locked)?;
        if !locked {
            return Ok(());
        }
        let username = self.terminal_lock_username.as_deref().unwrap_or_default();
        self.label("TerminalLockUser", &format!("系统账号：{username}"))?;
        self.set_visible("TerminalLockUser", !username.is_empty())?;
        // 对端没报用户名时才让用户自己填
        self.set_visible("TerminalLockUsername", username.is_empty())?;
        if !was {
            if let Some(password) =
                self.find_opt::<windows::UI::Xaml::Controls::PasswordBox>("TerminalLockPassword")
            {
                let control: Result<Control> = password.cast();
                if let Ok(control) = control {
                    let _ = control.Focus(FocusState::Programmatic);
                }
            }
        }
        Ok(())
    }

    /// 提交被控端系统密码（只校验，不解锁屏幕）。
    pub(crate) fn on_terminal_lock_submit(&mut self) -> Result<()> {
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        let password = self
            .find_opt::<windows::UI::Xaml::Controls::PasswordBox>("TerminalLockPassword")
            .and_then(|box_| box_.Password().ok())
            .map(|text| text.to_string())
            .unwrap_or_default();
        let username = self
            .find_opt::<TextBox>("TerminalLockUsername")
            .and_then(|box_| box_.Text().ok())
            .map(|text| text.to_string())
            .filter(|text| !text.trim().is_empty());
        if password.is_empty() {
            return self.label("TerminalLockError", "请输入被控端的系统登录密码");
        }
        self.label("TerminalLockError", "")?;
        match active.handle.submit_terminal_unlock(password, username) {
            Ok(()) => Ok(()),
            Err(err) => self.label("TerminalLockError", &err),
        }
    }

    /// 会话更新里与终端有关的几条：锁屏校验状态 / 结果、对方是否允许用终端。
    pub(crate) fn apply_terminal_update(&mut self, update: &SessionUpdate) -> Result<()> {
        use rc_session::proto::TOOL_CHANNEL_TERMINAL;
        match update {
            SessionUpdate::TerminalLockState {
                required,
                unlocked,
                username,
                retry_after_secs,
            } => {
                self.terminal_lock = TerminalLock {
                    required: *required,
                    unlocked: *unlocked,
                };
                self.terminal_lock_username = username.clone();
                if let Some(secs) = retry_after_secs.checked_sub(1).filter(|secs| *secs > 0) {
                    let _ = self.label(
                        "TerminalLockError",
                        &format!("尝试次数过多，请等待 {secs} 秒后再试"),
                    );
                }
                self.terminal_dirty = true;
            }
            SessionUpdate::TerminalUnlockResult {
                ok,
                error,
                retry_after_secs,
            } => {
                if *ok {
                    self.terminal_lock.unlocked = true;
                    if let Some(password) = self
                        .find_opt::<windows::UI::Xaml::Controls::PasswordBox>(
                            "TerminalLockPassword",
                        )
                    {
                        let _ = password.SetPassword(HSTRING::new());
                    }
                    let _ = self.label("TerminalLockError", "");
                    self.focus_terminal()?;
                } else {
                    let wait = if *retry_after_secs > 0 {
                        format!("（请等待 {} 秒）", retry_after_secs)
                    } else {
                        String::new()
                    };
                    let _ = self.label(
                        "TerminalLockError",
                        &format!(
                            "{}{wait}",
                            error.clone().unwrap_or_else(|| "密码不正确".to_string())
                        ),
                    );
                }
                self.terminal_dirty = true;
            }
            SessionUpdate::ToolPermissionDenied { channel, ui_error }
                if *channel == TOOL_CHANNEL_TERMINAL =>
            {
                self.terminal_allowed = false;
                let new_terminal: Control = self.find("TerminalNew")?;
                new_terminal.SetIsEnabled(false)?;
                return self
                    .set_terminal_hint(&format!("对方不允许使用终端：{}", ui_error.message));
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_is_picked_so_the_tracking_is_small() {
        // Consolas 的字宽约 0.55 em：13px 要补 6.5%，14px 只补 2.1%
        let consolas = pick_metrics(|size| size * 0.5498).expect("量得出字宽");
        assert_eq!(
            consolas,
            Metrics {
                font_size: 14.0,
                cell: 8.0,
                line: 19.0,
                tracking: 22
            }
        );
        // Cascadia Mono 约 0.6 em：13px 只补 1.5%
        let cascadia = pick_metrics(|size| size * 0.6).expect("量得出字宽");
        assert_eq!(
            cascadia,
            Metrics {
                font_size: 13.0,
                cell: 8.0,
                line: 18.0,
                tracking: 15
            }
        );
        // 量不出来就没有度量
        assert_eq!(pick_metrics(|_| 0.0), None);
    }

    #[test]
    fn thumb_maps_the_viewport_onto_the_track() {
        let range = ScrollRange {
            offset: 0,
            scrollable: 200,
            page: 50,
        };
        let track = 400.0;
        let thumb = thumb_length(track, range);
        assert!(
            (thumb - 80.0).abs() < 1e-9,
            "一屏占 50/250：滑块占轨道的 1/5"
        );
        assert_eq!(thumb_offset(track, thumb, range), 0.0);
        let end = ScrollRange {
            offset: 200,
            ..range
        };
        assert!((thumb_offset(track, thumb, end) - (track - thumb)).abs() < 1e-9);
        // 历史很长时滑块不短到抓不住
        let long = ScrollRange {
            offset: 10,
            scrollable: 100_000,
            page: 50,
        };
        assert_eq!(thumb_length(track, long), MIN_THUMB);
        // 没有历史：没有可滚动的距离
        assert_eq!(
            thumb_offset(
                track,
                MIN_THUMB,
                ScrollRange {
                    offset: 0,
                    scrollable: 0,
                    page: 50
                }
            ),
            0.0
        );
    }

    /// 在真实 XAML 运行时里加载会话窗口，把一条指令画进画布：验证 `session_window.xaml` 可解析、
    /// 元素池按需创建、多余的元素被收起。
    #[test]
    #[ignore] // 需要 XAML Islands 运行时：cargo test -p rc-ui terminal_canvas -- --ignored
    fn terminal_canvas_smoke() {
        use crate::biz::vt::{CURSOR, DEFAULT_FG};
        use windows::UI::Xaml::Controls::Panel;
        use windows::UI::Xaml::Hosting::WindowsXamlManager;
        use windows::UI::Xaml::Markup::XamlReader;

        let hr = unsafe {
            windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_SINGLETHREADED,
            )
        };
        assert!(hr.is_ok(), "RoInitialize: {hr:?}");
        let _manager =
            WindowsXamlManager::InitializeForCurrentThread().expect("XAML Islands 初始化");
        crate::app::install_styles().expect("合并样式字典");
        let root: FrameworkElement =
            XamlReader::Load(include_str!("../../ui/assets/session_window.xaml"))
                .expect("session_window.xaml 解析")
                .cast()
                .expect("应为 FrameworkElement");
        let bg: Panel = root
            .FindName("TerminalBg")
            .expect("TerminalBg")
            .cast()
            .expect("TerminalBg 是 Panel");
        let fg: Panel = root
            .FindName("TerminalFg")
            .expect("TerminalFg")
            .cast()
            .expect("TerminalFg 是 Panel");
        assert!(measure_metrics().is_some(), "量得出等宽字体的字宽");
        let metrics = Metrics {
            font_size: 14.0,
            cell: 8.0,
            line: 19.0,
            tracking: 30,
        };

        let ops = [
            Op::Rect {
                col: 3,
                cols: 1,
                color: CURSOR,
            },
            Op::Text {
                col: 0,
                text: "abc".to_string(),
                color: DEFAULT_FG,
                style: TextStyle::default(),
            },
        ];
        let mut pool = RowPool::default();
        let mut brushes = HashMap::new();
        draw_row(&ops, 0, &mut pool, &bg, &fg, metrics, &mut brushes).expect("画第 1 行");
        assert_eq!(
            bg.Children().expect("底层画布").Size().expect("背景条数"),
            1
        );
        assert_eq!(
            fg.Children().expect("上层画布").Size().expect("文本块数"),
            1
        );
        // 指令变少：多出来的元素收起而不是销毁
        draw_row(&[], 0, &mut pool, &bg, &fg, metrics, &mut brushes).expect("清空第 1 行");
        let first = bg
            .Children()
            .and_then(|children| children.GetAt(0))
            .expect("第 1 个背景条");
        assert_eq!(first.Visibility().expect("可见性"), Visibility::Collapsed);
        assert_eq!(pool.rects.len(), 1, "元素留在池里");
    }
}

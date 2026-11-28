//! 终端视图：[`VtScreen`] 之上的视口（历史滚动）、选区与绘制指令。
//!
//! 纯逻辑、无 GUI 依赖：XAML 渲染层只负责把 [`Op`] 逐条画到画布上。
//!
//! - **行号**用「绝对行号」= [`VtScreen::first_line`] + 缓冲区行号：历史满了从头部裁剪、复位清屏
//!   都不会让旧行号指向别的内容；
//! - **视口**：跟在底部时新输出来了自动往下走；向上翻看时锚定在视口首行，新输出在下面追加，
//!   不会把正在看的内容顶走；
//! - **选区**：锚点 + 焦点（焦点可以在锚点前面），按格取字，全角字符不会被截成半个；
//!   双击选词、三击选整行，之后拖动按同样的粒度延伸。

use std::cmp::Ordering;

use super::vt::{
    char_width, clip_cols, text_of, Rgb, Row, Style, VtScreen, CURSOR, DEFAULT_BG, DEFAULT_FG,
    SELECTION,
};

/// 光标格里的文字颜色（与 macOS 终端主题的 `cursorAccent` 一致）。
const CURSOR_TEXT: Rgb = DEFAULT_BG;
/// 输入焦点不在终端上时的光标色：暗一档的方块，只做位置提示。
const CURSOR_IDLE: Rgb = Rgb(0x22, 0x33, 0x66);
/// 光标闪烁周期（tick）：有按键 / 输出后先常亮一个周期，再按周期明暗交替。
const BLINK_TICKS: u64 = 12;
/// 网格尺寸的上下限（与 macOS 端一致；下限放宽，窄面板里不谎报列数）。
const MIN_COLS: usize = 4;
const MAX_GRID: usize = 512;
/// 选区端点的列号取这个值 = 整行。
const WHOLE_LINE: usize = usize::MAX;

/// 光标此刻怎么画。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// 不画（输入焦点不在终端上且正在闪烁的暗半周，或终端已经结束）。
    Hidden,
    /// 实心方块，格内文字换成反色。
    Solid,
    /// 输入焦点不在终端上：留一个暗色方块做位置提示，文字不变色。
    Idle,
}

/// 文本字型（暗淡已经折进前景色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextStyle {
    /// 粗体。
    pub bold: bool,
    /// 斜体。
    pub italic: bool,
    /// 下划线。
    pub underline: bool,
    /// 删除线。
    pub strike: bool,
}

impl From<Style> for TextStyle {
    fn from(style: Style) -> Self {
        Self {
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            strike: style.strike,
        }
    }
}

/// 绘制指令。同一行里先给背景条、再给文本，后者画在前者上面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// 背景条：从 `col` 列起、宽 `cols` 列，铺 `color`。
    Rect {
        /// 起始列（0 起）。
        col: usize,
        /// 占的列数。
        cols: usize,
        /// 填充色。
        color: Rgb,
    },
    /// 文本：左缘贴 `col` 列，前景 `color`，字型见 `style`。
    Text {
        /// 起始列（0 起）。
        col: usize,
        /// 文本（全角字符的指令恰好一个字符）。
        text: String,
        /// 前景色。
        color: Rgb,
        /// 字型。
        style: TextStyle,
    },
}

/// 一屏的绘制指令：每个视口行一组，行号 0 在最上。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// 各行的指令。
    pub rows: Vec<Vec<Op>>,
}

/// 滚动条几何（行数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollRange {
    /// 视口首行相对现存最早一行的偏移：0 在最上面。
    pub offset: usize,
    /// 还能往下滚的行数：0 = 没有历史（备用屏幕，或正好一屏）。
    pub scrollable: usize,
    /// 一屏行数。
    pub page: usize,
}

/// 选区：两端都是（绝对行号， 列号）；焦点可以在锚点前面。
/// 整行选中的端点列号是 [`WHOLE_LINE`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    /// 按下时的位置：选区延伸的基准，拖动过程中不变。
    origin: (usize, usize),
    anchor: (usize, usize),
    focus: (usize, usize),
}

impl Selection {
    fn line_range(&self) -> (usize, usize) {
        (
            self.anchor.0.min(self.focus.0),
            self.anchor.0.max(self.focus.0),
        )
    }

    /// 第 `line` 行被选中的列区间 `[start, end)`；这一行不在选区内为 `None`。
    /// 中间行、以及整行选中的端点，`end` 是 `usize::MAX`，由调用方夹到屏幕宽度。
    fn cols_on(&self, line: usize) -> Option<(usize, usize)> {
        let (top, bottom) = self.line_range();
        if line < top || line > bottom {
            return None;
        }
        // 两端各自的列号：同一行上按列排序（焦点可能在锚点左边），跨行时上行给起点、下行给终点
        let (start, end) = if top == bottom {
            (
                self.anchor.1.min(self.focus.1),
                self.anchor.1.max(self.focus.1),
            )
        } else if self.anchor.0 < self.focus.0 {
            (self.anchor.1, self.focus.1)
        } else {
            (self.focus.1, self.anchor.1)
        };
        Some((
            if line == top && start != WHOLE_LINE {
                start
            } else {
                0
            },
            if line == bottom { end } else { usize::MAX },
        ))
    }
}

/// 选区的延伸粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectMode {
    Chars,
    Words,
    Lines,
}

/// 光标闪烁（按 tick 计时，不读系统时钟）：有活动后常亮一个周期，之后按周期明暗交替。
#[derive(Debug, Default, Clone, Copy)]
pub struct Blink {
    last_active: u64,
}

impl Blink {
    /// 记一次活动（按键、输出）。
    pub fn touch(&mut self, tick: u64) {
        self.last_active = tick;
    }

    /// 此刻光标是否亮着。
    pub fn visible(&self, tick: u64) -> bool {
        tick.saturating_sub(self.last_active) / BLINK_TICKS % 2 == 0
    }
}

/// 终端视图：屏幕模型 + 视口 + 选区。
pub struct Terminal {
    screen: VtScreen,
    /// 视口首行的绝对行号；`None` = 跟在底部。
    top: Option<usize>,
    selection: Option<Selection>,
    select_mode: SelectMode,
    /// 离开底部那一刻的（总行数，光标行）：之后任一变了，就是翻看期间来了新输出。跟在底部时为 `None`。
    scrolled_from: Option<(usize, usize)>,
}

impl Terminal {
    /// 创建全空的终端视图。
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            screen: VtScreen::new(clamp_grid(cols, MIN_COLS), clamp_grid(rows, 1)),
            top: None,
            selection: None,
            select_mode: SelectMode::Chars,
            scrolled_from: None,
        }
    }

    /// 屏幕模型（只读：查询应答、括号粘贴 / 应用光标键模式等从这里取）。
    pub fn screen(&self) -> &VtScreen {
        &self.screen
    }

    /// 网格尺寸（列，行）。
    pub fn size(&self) -> (usize, usize) {
        self.screen.size()
    }

    /// 改变网格尺寸；返回是否变了（变了才需要通知远端）。
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        let size = (clamp_grid(cols, MIN_COLS), clamp_grid(rows, 1));
        if size == self.screen.size() {
            return false;
        }
        self.screen.resize(size.0, size.1);
        self.selection = None;
        true
    }

    /// 全量复位（新会话 / 终端退出）：清屏清历史、回到底部、清选区。
    pub fn reset(&mut self) {
        self.screen.full_reset();
        self.top = None;
        self.selection = None;
    }

    /// 一个字都没写过（缓冲区里的每一行都是空白）。
    ///
    /// 面板据此决定摆不摆「打开终端」的引导：终端结束后输出要留着给人看
    /// （退出信息、报错），这时再叠一块居中的引导只会糊在一起。
    pub fn is_blank(&self) -> bool {
        (0..self.screen.line_count()).all(|index| match self.screen.render_row(index) {
            Some(row) => row.spans.iter().all(|span| span.text.trim().is_empty()),
            None => true,
        })
    }

    /// 喂一段远端输出。
    pub fn feed_output(&mut self, bytes: &[u8]) {
        let was_alternate = self.screen.is_alternate();
        self.screen.feed_bytes(bytes);
        if was_alternate != self.screen.is_alternate() {
            // 备用屏幕没有历史可翻：进出时回到底部，旧选区对应的行已经不在了
            self.top = None;
            self.selection = None;
        }
    }

    /// 是否跟在底部（新输出来了视口自动往下走）。
    pub fn is_following(&self) -> bool {
        self.viewport_top() == self.last_line() + 1 - self.rows()
    }

    /// 视口首行的绝对行号。
    pub fn viewport_top(&self) -> usize {
        let bottom = self.last_line() + 1 - self.rows();
        // 备用屏幕没有历史可翻：永远只有主屏这几行
        if self.screen.is_alternate() {
            return bottom;
        }
        self.top
            .map_or(bottom, |top| top.clamp(self.screen.first_line(), bottom))
    }

    /// 滚动条几何：视口首行相对现存最早一行的偏移、还能往下滚的行数、一屏行数。
    pub fn scroll_range(&self) -> ScrollRange {
        ScrollRange {
            offset: self.viewport_top() - self.screen.first_line(),
            scrollable: self.scrollable(),
            page: self.rows(),
        }
    }

    /// 滚动 `lines` 行（负数向上翻历史）；回到最底下就重新跟随输出。
    pub fn scroll_by(&mut self, lines: isize) {
        let offset = self.scroll_range().offset as isize + lines;
        self.scroll_to_offset(offset.clamp(0, self.scrollable() as isize) as usize);
    }

    /// 滚动条滑块拖到相对现存最早一行的第 `offset` 行；拖到底就重新跟随输出。
    pub fn scroll_to_offset(&mut self, offset: usize) {
        // 备用屏幕没有历史可翻
        self.top = (offset < self.scrollable() && !self.screen.is_alternate())
            .then(|| self.screen.first_line() + offset);
        self.scrolled_from = self.top.map(|_| (self.last_line(), self.cursor_line()));
    }

    fn scrollable(&self) -> usize {
        self.screen.line_count() - self.rows()
    }

    /// 翻 `pages` 屏（负数向上）。
    pub fn scroll_pages(&mut self, pages: i8) {
        self.scroll_by(isize::from(pages) * (self.rows() - 1) as isize);
    }

    /// 回到最底下并重新跟随输出。
    pub fn scroll_to_bottom(&mut self) {
        self.top = None;
        self.scrolled_from = None;
    }

    /// 翻看历史期间是否来了新输出（总行数或光标行离开底部后变过）；跟在底部时恒为假。
    pub fn has_new_output(&self) -> bool {
        self.scrolled_from.is_some_and(|(lines, cursor)| {
            !self.is_following() && (lines != self.last_line() || cursor != self.cursor_line())
        })
    }

    /// 光标所在行的绝对行号。
    fn cursor_line(&self) -> usize {
        self.screen.first_line() + self.scrollable() + self.screen.cursor().0
    }

    /// 光标在视口里的位置（行，列）；光标行不在视口里为 `None`。输入框跟着它走，输入法的候选窗才出现在打字的位置。
    pub fn cursor_cell(&self) -> Option<(usize, usize)> {
        let (row, col) = self.screen.cursor();
        let line = self.screen.first_line() + self.scrollable() + row;
        line.checked_sub(self.viewport_top())
            .filter(|row| *row < self.rows())
            .map(|row| (row, col))
    }

    /// 取走屏幕模型积攒的查询应答（DA / DSR 等），由输入侧写回远端。
    pub fn take_responses(&mut self) -> Vec<u8> {
        self.screen.take_responses()
    }

    /// 从（绝对行号，列号）开始选：单击按格、双击选词、三击选整行，之后拖动按同样的粒度延伸。
    pub fn select_at(&mut self, at: (usize, usize), clicks: u8) {
        let (anchor, focus) = match clicks {
            1 => (at, at),
            2 => {
                let (start, end) = word_at(&self.line_text(at.0), at.1);
                ((at.0, start), (at.0, end))
            }
            _ => ((at.0, WHOLE_LINE), (at.0, WHOLE_LINE)),
        };
        self.select_mode = match clicks {
            1 => SelectMode::Chars,
            2 => SelectMode::Words,
            _ => SelectMode::Lines,
        };
        self.selection = Some(Selection {
            origin: at,
            anchor,
            focus,
        });
    }

    /// 把选区焦点移到（绝对行号，列号），粒度沿用开始选择时的。
    pub fn extend_selection(&mut self, to: (usize, usize)) {
        let Some(selection) = self.selection else {
            return;
        };
        let origin = selection.origin;
        let (anchor, focus) = match self.select_mode {
            SelectMode::Chars => (origin, to),
            SelectMode::Lines => ((origin.0, WHOLE_LINE), (to.0, WHOLE_LINE)),
            SelectMode::Words => {
                let (anchor_start, anchor_end) = word_at(&self.line_text(origin.0), origin.1);
                let (start, end) = word_at(&self.line_text(to.0), to.1);
                match to.0.cmp(&origin.0) {
                    // 往下拖：指针左边的内容都在选区里，焦点行选到词尾
                    Ordering::Greater => ((origin.0, anchor_start), (to.0, end)),
                    // 往上拖：指针右边的内容都在选区里，焦点行从词头起
                    Ordering::Less => ((origin.0, anchor_end), (to.0, start)),
                    // 同一行：起点所在的词与指针所在的词，两头里靠外的那头
                    Ordering::Equal if start >= anchor_start => {
                        ((to.0, anchor_start), (to.0, end.max(anchor_end)))
                    }
                    Ordering::Equal => ((to.0, anchor_end), (to.0, start.min(anchor_start))),
                }
            }
        };
        self.selection = Some(Selection {
            origin,
            anchor,
            focus,
        });
    }

    /// 取消选区。
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// 是否有选区。
    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// 选区文本：行之间以 CRLF 分隔，每行去掉行尾空白；没有选区为空串。
    pub fn selected_text(&self) -> String {
        let Some(selection) = self.selection else {
            return String::new();
        };
        let first = self.screen.first_line();
        let (top, bottom) = selection.line_range();
        (top..=bottom)
            .filter_map(|line| {
                let row = self.screen.render_row(line.checked_sub(first)?)?;
                let (start, end) = selection.cols_on(line)?;
                let mut text = clip_cols(&text_of(&row.spans), start, end);
                while text.ends_with(' ') {
                    text.pop();
                }
                Some(text)
            })
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    /// 粘贴文本 → 发给远端的字节：换行统一成 CR；远端开了括号粘贴（`?2004`）就包起来。
    pub fn paste_bytes(&self, text: &str) -> Vec<u8> {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.screen.bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        }
    }

    /// 当前视口的一屏绘制指令，光标按 `cursor` 画。
    pub fn frame(&self, cursor: Cursor) -> Frame {
        let first = self.screen.first_line();
        let (cols, rows) = self.screen.size();
        let top = self.viewport_top();
        Frame {
            rows: (0..rows)
                .map(|row| {
                    let line = top + row;
                    let data = self
                        .screen
                        .render_row(line - first)
                        .expect("视口行落在缓冲区之内");
                    let selected = self
                        .selection
                        .as_ref()
                        .and_then(|selection| selection.cols_on(line))
                        .map(|(start, end)| (start.min(cols), end.min(cols)));
                    row_ops(&data, selected, cursor)
                })
                .collect(),
        }
    }

    fn rows(&self) -> usize {
        self.screen.size().1
    }

    /// 最后一行的绝对行号。
    fn last_line(&self) -> usize {
        self.screen.first_line() + self.screen.line_count() - 1
    }

    /// 第 `line` 行的纯文本；已经不在缓冲区里的行为空串。
    fn line_text(&self, line: usize) -> String {
        line.checked_sub(self.screen.first_line())
            .and_then(|index| self.screen.render_row(index))
            .map_or_else(String::new, |row| text_of(&row.spans))
    }
}

fn clamp_grid(value: usize, min: usize) -> usize {
    value.clamp(min, MAX_GRID)
}

/// 一行的绘制指令：底色条 → 选区条 → 光标条 → 文本。
///
/// - 相邻且同底色的段连成一条；默认底色不画（画布本身就是默认底色）；
/// - 全角字符各自成一条文本指令：它的字形宽度取决于字体回退，后面的内容必须从确定的列重新起头；
/// - 光标格里的文字换成光标的反色，看起来才是「实心方块盖住了一个字」。
fn row_ops(row: &Row, selected: Option<(usize, usize)>, cursor_state: Cursor) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut runs: Vec<(usize, usize, Rgb)> = Vec::new();
    for span in &row.spans {
        let (_, bg) = span.style.resolve_pair(DEFAULT_FG, DEFAULT_BG);
        let end = span.col + span.cols();
        match runs.last_mut() {
            Some((_, run_end, color)) if *run_end == span.col && *color == bg => *run_end = end,
            _ => runs.push((span.col, end, bg)),
        }
    }
    for (col, end, color) in runs {
        if color != DEFAULT_BG {
            ops.push(Op::Rect {
                col,
                cols: end - col,
                color,
            });
        }
    }

    let cursor = row.cursor.filter(|_| cursor_state != Cursor::Hidden);
    if let Some((mut start, mut end)) = selected {
        // 选区边界落在全角字符中间：整个字符一起选中，不留半个
        for span in row.spans.iter().filter(|span| span.wide) {
            if start > span.col && start < span.col + 2 {
                start = span.col;
            }
            if end > span.col && end < span.col + 2 {
                end = span.col + 2;
            }
        }
        ops.push(Op::Rect {
            col: start,
            cols: end - start,
            color: SELECTION,
        });
    }
    if let Some((col, wide)) = cursor {
        ops.push(Op::Rect {
            col,
            cols: usize::from(wide) + 1,
            color: if cursor_state == Cursor::Solid {
                CURSOR
            } else {
                CURSOR_IDLE
            },
        });
    }

    // 只有实心光标盖住的文字才换成反色；提示用的暗色方块上文字照常
    let text_color = |col: usize, fg: Rgb| match cursor {
        Some((cursor_col, wide))
            if cursor_state == Cursor::Solid
                && col >= cursor_col
                && col <= cursor_col + usize::from(wide) =>
        {
            CURSOR_TEXT
        }
        _ => fg,
    };
    // 连续文本：同一前景色与字型、列号首尾相接的字符并成一条指令
    let mut run: Option<(usize, String, Rgb, TextStyle)> = None;
    let mut run_end = 0;
    for span in &row.spans {
        let (fg, _) = span.style.resolve_pair(DEFAULT_FG, DEFAULT_BG);
        let style = TextStyle::from(span.style);
        if span.col != run_end {
            // 段之间有空隙（理论上不会发生）：空隙后的内容从它自己的列重新起头
            flush(&mut run, &mut ops);
        }
        if span.wide {
            flush(&mut run, &mut ops);
            ops.push(Op::Text {
                col: span.col,
                text: span.text.clone(),
                color: text_color(span.col, fg),
                style,
            });
            run_end = span.col + 2;
            continue;
        }
        for ch in span.text.chars() {
            let width = char_width(ch);
            if width == 0 {
                // 组合字符跟着它修饰的那个字符走
                if let Some((_, text, _, _)) = run.as_mut() {
                    text.push(ch);
                }
                continue;
            }
            let color = text_color(run_end, fg);
            let joins = run.as_ref().is_some_and(|(_, _, run_color, run_style)| {
                *run_color == color && *run_style == style
            });
            if !joins {
                flush(&mut run, &mut ops);
                run = Some((run_end, String::new(), color, style));
            }
            if let Some((_, text, _, _)) = run.as_mut() {
                text.push(ch);
            }
            run_end += width;
        }
    }
    flush(&mut run, &mut ops);
    ops
}

fn flush(run: &mut Option<(usize, String, Rgb, TextStyle)>, ops: &mut Vec<Op>) {
    if let Some((col, text, color, style)) = run.take() {
        ops.push(Op::Text {
            col,
            text,
            color,
            style,
        });
    }
}

/// `text` 里第 `col` 列所在字符的列区间 `[start, end)`：以空白与常见标点为界；
/// 点在分隔符上就只选它自己，点在行尾之外是空区间。
pub fn word_at(text: &str, col: usize) -> (usize, usize) {
    // （起始列，字符，宽度）——组合字符不占列，不算格
    let cells: Vec<(usize, char, usize)> = text
        .chars()
        .scan(0usize, |start, ch| {
            let at = *start;
            let width = char_width(ch);
            *start += width;
            Some((at, ch, width))
        })
        .filter(|(_, _, width)| *width > 0)
        .collect();
    let Some(index) = cells
        .iter()
        .position(|&(start, _, width)| col >= start && col < start + width)
    else {
        return (col, col);
    };
    let (start, ch, width) = cells[index];
    if !is_word_char(ch) {
        return (start, start + width);
    }
    let first = cells[..index]
        .iter()
        .rposition(|(_, ch, _)| !is_word_char(*ch))
        .map_or(0, |previous| previous + 1);
    let last = cells[index + 1..]
        .iter()
        .position(|(_, ch, _)| !is_word_char(*ch))
        .map_or(cells.len(), |next| index + 1 + next);
    let (last_start, _, last_width) = cells[last - 1];
    (cells[first].0, last_start + last_width)
}

/// 选词时的「词字符」：字母数字（含 CJK）与路径 / 参数里常见的符号。
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || "_-./:@#%~+=\\".contains(ch)
}

#[cfg(test)]
mod tests {
    use super::super::vt;
    use super::*;

    fn red() -> Rgb {
        vt::palette(1)
    }
    fn green() -> Rgb {
        vt::palette(2)
    }
    fn blue() -> Rgb {
        vt::palette(4)
    }

    fn rect(col: usize, cols: usize, color: Rgb) -> Op {
        Op::Rect { col, cols, color }
    }
    fn text(col: usize, text: &str, color: Rgb) -> Op {
        Op::Text {
            col,
            text: text.to_string(),
            color,
            style: TextStyle::default(),
        }
    }

    /// 一屏的第 `row` 行指令。
    fn row_of(terminal: &Terminal, row: usize) -> Vec<Op> {
        terminal.frame(Cursor::Solid).rows[row].clone()
    }

    #[test]
    fn background_runs_merge_and_the_default_is_not_drawn() {
        let mut terminal = Terminal::new(10, 2);
        terminal.feed_output(b"\x1b[41m  \x1b[42m x\x1b[44m\x1b[1mhi\x1b[0m");
        assert_eq!(
            row_of(&terminal, 0),
            [
                rect(0, 2, red()),
                rect(2, 2, green()),
                rect(4, 2, blue()),
                rect(6, 1, CURSOR),
                Op::Text {
                    col: 0,
                    text: "   x".to_string(),
                    color: DEFAULT_FG,
                    style: TextStyle::default(),
                },
                Op::Text {
                    col: 4,
                    text: "hi".to_string(),
                    color: DEFAULT_FG,
                    style: TextStyle {
                        bold: true,
                        ..TextStyle::default()
                    },
                },
            ],
            "先底色条、再光标条、最后文本；同底色的空格并成一条；前景相同的相邻段并成一条文本，粗体处断开"
        );
    }

    #[test]
    fn cursor_is_a_block_under_accented_text() {
        let mut terminal = Terminal::new(10, 2);
        terminal.feed_output(b"ab");
        assert_eq!(
            row_of(&terminal, 0),
            [rect(2, 1, CURSOR), text(0, "ab", DEFAULT_FG)],
            "光标在行尾的空格上"
        );
        terminal.feed_output(b"\r");
        assert_eq!(
            row_of(&terminal, 0),
            [
                rect(0, 1, CURSOR),
                text(0, "a", CURSOR_TEXT),
                text(1, "b", DEFAULT_FG)
            ],
            "光标盖住 a：a 换成反色"
        );
        assert_eq!(
            terminal.frame(Cursor::Hidden).rows[0],
            [text(0, "ab", DEFAULT_FG)],
            "不画光标时文本并回一条"
        );
        // 失焦：暗色方块提示位置，文字不变色
        assert_eq!(
            terminal.frame(Cursor::Idle).rows[0],
            [rect(0, 1, CURSOR_IDLE), text(0, "ab", DEFAULT_FG)]
        );
        terminal.feed_output(b"\x1b[?25l");
        assert_eq!(
            terminal.frame(Cursor::Solid).rows[0],
            [text(0, "ab", DEFAULT_FG)],
            "远端隐藏了光标：焦点在也不画"
        );
    }

    #[test]
    fn wide_characters_get_their_own_cells() {
        let mut terminal = Terminal::new(20, 2);
        terminal.feed_output("中文abc".as_bytes());
        assert_eq!(
            row_of(&terminal, 0),
            [
                rect(7, 1, CURSOR),
                text(0, "中", DEFAULT_FG),
                text(2, "文", DEFAULT_FG),
                text(4, "abc", DEFAULT_FG)
            ],
            "abc 占第 4–6 列，光标落在第 7 列"
        );
        terminal.feed_output(b"\x1b[1;3H");
        assert_eq!(
            row_of(&terminal, 0),
            [
                rect(2, 2, CURSOR),
                text(0, "中", DEFAULT_FG),
                text(2, "文", CURSOR_TEXT),
                text(4, "abc", DEFAULT_FG),
            ],
            "光标落在全角字符上占两格"
        );
    }

    #[test]
    fn selection_is_a_strip_over_the_background_and_snaps_to_wide_cells() {
        let mut terminal = Terminal::new(20, 2);
        terminal.feed_output("\x1b[41m中文abc\x1b[0m".as_bytes());
        terminal.select_at((0, 1), 1);
        terminal.extend_selection((0, 5));
        assert_eq!(
            row_of(&terminal, 0),
            [
                rect(0, 7, red()),
                rect(0, 5, SELECTION),
                rect(7, 1, CURSOR),
                text(0, "中", DEFAULT_FG),
                text(2, "文", DEFAULT_FG),
                text(4, "abc", DEFAULT_FG),
            ],
            "选区从「中」的中间开始：整个字一起选中"
        );
    }

    #[test]
    fn selected_text_clips_columns_and_trims_trailing_spaces() {
        let mut terminal = Terminal::new(12, 3);
        terminal.feed_output("hello world\r\n中文测试 x\r\nlast".as_bytes());
        terminal.select_at((0, 0), 1);
        terminal.extend_selection((0, 5));
        assert_eq!(terminal.selected_text(), "hello");
        terminal.select_at((0, 6), 1);
        terminal.extend_selection((1, 4));
        assert_eq!(terminal.selected_text(), "world\r\n中文");
        // 焦点在锚点前面：结果一样
        terminal.select_at((1, 4), 1);
        terminal.extend_selection((0, 6));
        assert_eq!(terminal.selected_text(), "world\r\n中文");
        // 选到行尾之外：行尾空白去掉
        terminal.select_at((1, 0), 1);
        terminal.extend_selection((1, 12));
        assert_eq!(terminal.selected_text(), "中文测试 x");
        terminal.clear_selection();
        assert_eq!(terminal.selected_text(), "");
        assert!(!terminal.has_selection());
    }

    #[test]
    fn double_click_selects_words_and_triple_click_selects_lines() {
        let mut terminal = Terminal::new(20, 2);
        terminal.feed_output("foo-bar baz 中x  ".as_bytes());
        terminal.select_at((0, 2), 2);
        assert_eq!(terminal.selected_text(), "foo-bar");
        terminal.extend_selection((0, 9));
        assert_eq!(terminal.selected_text(), "foo-bar baz");
        terminal.extend_selection((0, 13));
        assert_eq!(
            terminal.selected_text(),
            "foo-bar baz 中x",
            "汉字与紧挨着的字母之间没有分隔符：算一个词"
        );
        // 往回调头
        terminal.select_at((0, 9), 2);
        terminal.extend_selection((0, 2));
        assert_eq!(terminal.selected_text(), "foo-bar baz");
        terminal.extend_selection((0, 10));
        assert_eq!(
            terminal.selected_text(),
            "baz",
            "回到锚点所在的词：只选这个词"
        );
        terminal.select_at((0, 13), 3);
        terminal.extend_selection((0, 1));
        assert_eq!(terminal.selected_text(), "foo-bar baz 中x", "三击选整行");
        // 点在标点上只选它自己；点在行尾之外是空的
        let mut terminal = Terminal::new(20, 2);
        terminal.feed_output("f(x) 中".as_bytes());
        terminal.select_at((0, 1), 2);
        assert_eq!(terminal.selected_text(), "(");
        terminal.extend_selection((0, 3));
        assert_eq!(
            terminal.selected_text(),
            "(x)",
            "同一行：起点与指针所在的两个「词」连同中间的"
        );
        terminal.select_at((0, 19), 2);
        assert!(terminal.has_selection());
        assert_eq!(terminal.selected_text(), "");
        // 往下 / 往上拖到别的行：按词尾 / 词头收
        let mut terminal = Terminal::new(20, 4);
        terminal.feed_output("one two three\r\nfour five six\r\nseven eight\r\n".as_bytes());
        terminal.select_at((1, 1), 2);
        terminal.extend_selection((2, 10));
        assert_eq!(
            terminal.selected_text(),
            "four five six\r\nseven eight",
            "往下拖：末行选到词尾"
        );
        terminal.extend_selection((0, 11));
        assert_eq!(
            terminal.selected_text(),
            "three\r\nfour",
            "往上拖：首行从词头起，锚点所在行选到锚点所在的词为止"
        );
        terminal.select_at((1, 1), 3);
        terminal.extend_selection((2, 0));
        assert_eq!(terminal.selected_text(), "four five six\r\nseven eight");
        terminal.extend_selection((0, 0));
        assert_eq!(
            terminal.selected_text(),
            "one two three\r\nfour five six",
            "整行选区往上调头"
        );
    }

    #[test]
    fn scrolling_holds_the_view_on_history_until_it_returns_to_the_bottom() {
        let mut terminal = Terminal::new(10, 3);
        for n in 1..=7 {
            terminal.feed_output(format!("line{n}\r\n").as_bytes());
        }
        assert!(terminal.is_following());
        assert!(
            !terminal.has_new_output(),
            "跟着底部时来的输出看得到，不用提示"
        );
        assert_eq!(
            terminal.scroll_range(),
            ScrollRange {
                offset: 5,
                scrollable: 5,
                page: 3
            },
            "跟在底部：偏移 = 可滚动的全部 5 行"
        );
        terminal.scroll_by(-3);
        assert!(!terminal.is_following());
        assert_eq!(terminal.scroll_range().offset, 2);
        let before = terminal.frame(Cursor::Solid);
        // 新输出在下面追加：正在看的内容不动；翻看期间新到的行记下来，用来提示「下面有新输出」
        for n in 8..=9 {
            terminal.feed_output(format!("line{n}\r\n").as_bytes());
        }
        assert_eq!(terminal.frame(Cursor::Solid), before);
        assert!(terminal.has_new_output(), "翻看期间来了新输出");
        // 新输出落在主屏内、没有整行滚出历史：光标行变了，同样算
        let mut in_screen = Terminal::new(10, 6);
        for n in 1..=9 {
            in_screen.feed_output(format!("l{n}\r\n").as_bytes());
        }
        in_screen.scroll_by(-2);
        assert!(!in_screen.has_new_output(), "只是翻上去看，还没有新输出");
        in_screen.feed_output(b"\x1b[3A\r\n\r\nx");
        assert!(
            in_screen.has_new_output(),
            "光标往上 3 行再往下 2 行都在主屏内：光标行变了就算"
        );
        assert_eq!(
            terminal.scroll_range(),
            ScrollRange {
                offset: 2,
                scrollable: 7,
                page: 3
            }
        );
        // 滚过头：夹在最底下，重新跟随
        terminal.scroll_by(100);
        assert!(terminal.is_following());
        assert!(!terminal.has_new_output(), "回到底部就不再提示");
        assert_eq!(terminal.scroll_range().offset, 7);
        // 翻页：一屏 = 行数 − 1
        terminal.scroll_pages(-1);
        assert_eq!(terminal.scroll_range().offset, 5);
        assert!(!terminal.is_following());
        terminal.scroll_pages(1);
        assert!(terminal.is_following());
        // 向上滚出范围：夹在现存最早的一行
        terminal.scroll_by(-100);
        assert_eq!(terminal.scroll_range().offset, 0);
        assert!(!terminal.is_following());
        terminal.scroll_by(-2);
        terminal.scroll_to_bottom();
        assert!(terminal.is_following());
        // 滚动条滑块：按相对偏移定位，拖到底重新跟随
        terminal.scroll_to_offset(3);
        assert_eq!(terminal.viewport_top(), 3);
        assert!(!terminal.is_following());
        terminal.scroll_to_offset(999);
        assert!(terminal.is_following());
        // 光标在视口里的位置（输入框跟着它走）；翻到历史里就找不到
        assert_eq!(terminal.cursor_cell(), Some((2, 0)));
        terminal.scroll_by(-3);
        assert_eq!(terminal.cursor_cell(), None);
    }

    #[test]
    fn query_responses_are_handed_over_once() {
        let mut terminal = Terminal::new(10, 2);
        terminal.feed_output(b"\x1b[6n");
        assert_eq!(terminal.take_responses(), b"\x1b[1;1R".to_vec());
        assert!(terminal.take_responses().is_empty());
    }

    #[test]
    fn cursor_row_is_the_only_row_with_a_cursor_and_hidden_rows_have_none() {
        let mut terminal = Terminal::new(10, 3);
        for n in 1..=7 {
            terminal.feed_output(format!("line{n}\r\n").as_bytes());
        }
        let frame = terminal.frame(Cursor::Solid);
        let cursors: Vec<usize> = frame
            .rows
            .iter()
            .enumerate()
            .filter(|(_, ops)| {
                ops.iter()
                    .any(|op| matches!(op, Op::Rect { color, .. } if *color == CURSOR))
            })
            .map(|(row, _)| row)
            .collect();
        assert_eq!(cursors, [2], "视口 = 第 6 行、第 7 行、光标所在的空行");
        terminal.scroll_by(-3);
        assert!(
            terminal.frame(Cursor::Solid).rows.iter().all(|ops| !ops
                .iter()
                .any(|op| matches!(op, Op::Rect { color, .. } if *color == CURSOR))),
            "翻到历史里：光标行不在视口里"
        );
    }

    #[test]
    fn dropped_history_never_reuses_line_numbers() {
        let mut terminal = Terminal::new(10, 2);
        for n in 0..vt::SCROLLBACK_CAP + 12 {
            terminal.feed_output(format!("line{n}\r\n").as_bytes());
        }
        assert_eq!(
            terminal.screen().first_line(),
            11,
            "超出上限的历史从头部裁掉"
        );
        assert_eq!(
            terminal.scroll_range(),
            ScrollRange {
                offset: 1000,
                scrollable: 1000,
                page: 2
            }
        );
        // 钉在一行很早的历史上，再被裁掉：视口夹到现存最早的一行，选区里已经不在缓冲区的行取不到字
        terminal.select_at((11, 0), 3);
        terminal.scroll_by(-100);
        assert_eq!(terminal.viewport_top(), 911, "从底部向上翻 100 行");
        assert_eq!(terminal.selected_text(), "line11");
        terminal.scroll_by(-1_000_000);
        assert_eq!(terminal.viewport_top(), 11, "夹在现存最早的一行");
        for n in 1012..1032 {
            terminal.feed_output(format!("line{n}\r\n").as_bytes());
        }
        assert_eq!(terminal.screen().first_line(), 31);
        assert_eq!(
            terminal.viewport_top(),
            31,
            "原先钉住的那行被裁掉了：跟着夹到新的最早一行"
        );
        assert_eq!(terminal.selected_text(), "", "选区那一行已经被裁掉");
    }

    #[test]
    fn alternate_screen_forces_follow_and_clears_selection() {
        let mut terminal = Terminal::new(10, 3);
        terminal.feed_output(b"a\r\nb\r\nc\r\nd\r\ne");
        terminal.scroll_by(-2);
        terminal.select_at((1, 0), 3);
        assert!(!terminal.is_following());
        terminal.feed_output(b"\x1b[?1049h");
        assert!(terminal.is_following());
        assert!(!terminal.has_selection());
        assert_eq!(terminal.frame(Cursor::Solid).rows.len(), 3);
        terminal.feed_output(b"\x1b[?1049l");
        assert!(terminal.is_following(), "回到主屏后从底部接着走");
    }

    #[test]
    fn resize_and_reset_start_over() {
        let mut terminal = Terminal::new(20, 4);
        terminal.feed_output(b"hello\r\nworld");
        terminal.scroll_by(-1);
        terminal.select_at((0, 0), 3);
        assert!(!terminal.resize(20, 4), "尺寸没变不用通知远端");
        assert!(terminal.resize(12, 3));
        assert_eq!(terminal.size(), (12, 3));
        assert!(terminal.is_following(), "变小后原来的视口贴到了底");
        assert!(!terminal.has_selection());
        assert!(terminal.resize(1, 1));
        assert_eq!(terminal.size(), (4, 1), "下限：至少 4 列 1 行");
        assert!(terminal.resize(9999, 9999));
        assert_eq!(terminal.size(), (512, 512), "上限与 macOS 端一致");
        terminal.reset();
        assert_eq!(terminal.screen().line_count(), 512);
        assert!(terminal.is_following());
    }

    #[test]
    fn paste_normalizes_newlines_and_wraps_when_the_remote_asks() {
        let mut terminal = Terminal::new(10, 2);
        assert_eq!(terminal.paste_bytes("a\r\nb\nc"), b"a\rb\rc".to_vec());
        terminal.feed_output(b"\x1b[?2004h");
        assert_eq!(
            terminal.paste_bytes("ls\n"),
            b"\x1b[200~ls\r\x1b[201~".to_vec()
        );
    }

    #[test]
    fn blink_stays_on_after_activity_then_alternates() {
        let mut blink = Blink::default();
        assert!(blink.visible(0));
        blink.touch(100);
        for tick in 100..112 {
            assert!(blink.visible(tick), "tick {tick}");
        }
        assert!(!blink.visible(112));
        assert!(!blink.visible(123));
        assert!(blink.visible(124));
        blink.touch(130);
        assert!(blink.visible(130), "一有活动立刻点亮");
    }

    #[test]
    fn combining_marks_stay_with_their_base_and_columns_hold() {
        let mut terminal = Terminal::new(10, 2);
        terminal.feed_output("e\u{301}x".as_bytes());
        assert_eq!(
            row_of(&terminal, 0),
            [rect(2, 1, CURSOR), text(0, "e\u{301}x", DEFAULT_FG),],
            "组合字符不占列：x 仍在第 1 列，光标在第 2 列"
        );
        assert_eq!(terminal.screen().render_row(0).unwrap().spans[0].cols(), 2);
    }

    #[test]
    fn blank_until_something_visible_is_written() {
        let mut terminal = Terminal::new(10, 3);
        assert!(terminal.is_blank(), "新终端一个字都没有");
        terminal.feed_output(b"\x1b[2J\x1b[H  \t\r\n\n");
        assert!(terminal.is_blank(), "清屏、换行和空白不算写过字");
        terminal.feed_output(b"\x1b[2J");
        assert!(terminal.is_blank(), "清屏把历史一起清掉，又是空白");
        terminal.feed_output(b"$ ");
        assert!(!terminal.is_blank(), "有了提示符就不是空白");
        // 终端结束时远端会补一行结束信息：屏幕上一定留着字，引导不该叠上来
        terminal.feed_output("\r\n\x1b[90m[终端 #0 已结束]\x1b[0m\r\n".as_bytes());
        assert!(!terminal.is_blank(), "结束信息还留在屏幕上");
        terminal.reset();
        assert!(terminal.is_blank(), "全量复位后回到空白");
    }
}

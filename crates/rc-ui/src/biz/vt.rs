//! VT100/xterm 转义序列解析与屏幕模型（终端工具的渲染数据源）。
//!
//! 纯逻辑、无 GUI 依赖：输入远端终端的输出字节，输出「按列定位」的文本行供 XAML 渲染。
//!
//! # 支持范围
//!
//! - **列宽**：CJK 全角字符占 2 列（Unicode 宽度表，与 macOS 端 xterm.js 同一口径），
//!   组合字符（音标、变音符号、零宽 joiner）不占列、跟随前一个字符；
//! - SGR：粗体 / 暗淡 / 斜体 / 下划线 / 删除线 / 反显，前景背景 16 色 / 256 色 / 真彩色，
//!   冒号形式的子参数（`38:5:n`）；
//! - 光标：上下左右、绝对定位、保存 / 恢复、显隐（`?25`）；CR / LF / BS / Tab；
//! - 编辑：擦除显示 / 行 / 字符，插入 / 删除字符 / 行；
//! - 滚动区（DECSTBM）与区内滚动、正向 / 反向换行；
//! - **备用屏幕**（`?1049` / `?47`）：全屏程序（vim / less）在备用屏上画，退出后主屏原样回来；
//! - **模式跟踪**：括号粘贴（`?2004`）、应用光标键（`?1`），供输入侧按远端的期望编码；
//! - **查询应答**：DA1 / DA2 / DSR（状态、光标位置）排进 [`VtScreen::take_responses`]，
//!   由输入侧发回远端（依赖应答的全屏程序不必干等）；
//! - OSC（窗口标题等）整体丢弃。
//!
//! # 渲染契约
//!
//! [`VtScreen::render_rows`] 给出每一行的若干 [`Span`]：`col` 是该段起始列，`wide` 为真的段
//! 恰好一个全角字符（占 2 列），否则每个字符 1 列。相邻的段列号首尾相接，渲染端据此按列
//! 定位文本，并为连续同底色的段画背景矩形——文本与底色都落在精确的列格上，CJK 不会让后面的
//! 内容错位。
//!
//! # 已知限制
//!
//! - 换行是立即换（没有 xterm 的「停在最后一列等下一个字符」延迟换行）；
//! - 不应答 OSC 查询（颜色 / 剪贴板），不处理鼠标上报、源模式（`?6`）；
//! - 尺寸变化不重排（截断 / 补空，不做 reflow）。

#![deny(missing_docs)]

use unicode_width::UnicodeWidthChar;

/// 默认前景色（与会话窗口的深色令牌一致，偏冷的浅灰）。
pub const DEFAULT_FG: Rgb = Rgb(0xE6, 0xED, 0xF3);
/// 默认背景色（与会话窗口里终端画面的底色 `DarkTerminalBrush` 一致）。
pub const DEFAULT_BG: Rgb = Rgb(0x0C, 0x0F, 0x14);
/// 光标颜色（品牌蓝，与会话窗口的强调色一致）。
pub const CURSOR: Rgb = Rgb(0x34, 0x6C, 0xFF);
/// 选区底色（品牌蓝叠在终端底色上约三成）。
pub const SELECTION: Rgb = Rgb(0x22, 0x35, 0x6F);
/// 滚动历史行数上限。
pub const SCROLLBACK_CAP: usize = 1000;
/// CSI 缓冲上限（防失控序列撑爆内存）。
const CSI_CAP: usize = 128;

/// RGB 颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `0xRRGGBB`（供 XAML 画刷）。
    pub const fn to_hex(self) -> u32 {
        ((self.0 as u32) << 16) | ((self.1 as u32) << 8) | self.2 as u32
    }
}

/// 终端调色板索引或真彩色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// 0–255 调色板索引。
    Indexed(u8),
    /// 真彩色。
    Rgb(Rgb),
}

/// 单元格样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// 前景色；`None` = 默认。
    pub fg: Option<Color>,
    /// 背景色；`None` = 默认。
    pub bg: Option<Color>,
    /// 粗体（标准 8 色同时提亮为亮色）。
    pub bold: bool,
    /// 暗淡。
    pub dim: bool,
    /// 斜体。
    pub italic: bool,
    /// 下划线。
    pub underline: bool,
    /// 删除线。
    pub strike: bool,
    /// 反显（前景背景交换）。
    pub inverse: bool,
}

impl Style {
    /// 前景 / 背景色对：反显交换两者，粗体提亮标准色，暗淡把前景向背景折半。
    pub fn resolve_pair(self, default_fg: Rgb, default_bg: Rgb) -> (Rgb, Rgb) {
        let fg = self.fg.map_or(default_fg, |c| Self::resolve(c, self.bold));
        let bg = self.bg.map_or(default_bg, |c| Self::resolve(c, false));
        let (fg, bg) = if self.inverse { (bg, fg) } else { (fg, bg) };
        if self.dim {
            (mix(fg, bg), bg)
        } else {
            (fg, bg)
        }
    }

    fn resolve(color: Color, bold: bool) -> Rgb {
        match color {
            Color::Rgb(rgb) => rgb,
            Color::Indexed(n) => palette(if bold && n < 8 { n + 8 } else { n }),
        }
    }
}

/// xterm 256 色调色板；前 16 色是深色底上辨识度高的现代配色（同 GitHub Dark）。
pub fn palette(index: u8) -> Rgb {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    const BASIC: [Rgb; 16] = [
        Rgb(0x5C, 0x67, 0x73),
        Rgb(0xFF, 0x7B, 0x72),
        Rgb(0x3F, 0xB9, 0x50),
        Rgb(0xD2, 0x99, 0x22),
        Rgb(0x58, 0xA6, 0xFF),
        Rgb(0xBC, 0x8C, 0xFF),
        Rgb(0x39, 0xC5, 0xCF),
        Rgb(0xB1, 0xBA, 0xC4),
        Rgb(0x7D, 0x87, 0x94),
        Rgb(0xFF, 0xA1, 0x98),
        Rgb(0x56, 0xD3, 0x64),
        Rgb(0xE3, 0xB3, 0x41),
        Rgb(0x79, 0xC0, 0xFF),
        Rgb(0xD2, 0xA8, 0xFF),
        Rgb(0x56, 0xD4, 0xDD),
        Rgb(0xF0, 0xF6, 0xFC),
    ];
    match index {
        0..=15 => BASIC[index as usize],
        16..=231 => {
            let i = index - 16;
            Rgb(
                LEVELS[(i / 36) as usize],
                LEVELS[((i % 36) / 6) as usize],
                LEVELS[(i % 6) as usize],
            )
        }
        gray => {
            let v = 8 + (gray - 232) * 10;
            Rgb(v, v, v)
        }
    }
}

/// 按通道线性混合 `a`、`b`（`t` = 0 取 `a`，1 取 `b`）。
pub fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let channel = |x: u8, y: u8| (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8;
    Rgb(channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
}

fn mix(a: Rgb, b: Rgb) -> Rgb {
    blend(a, b, 0.5)
}

/// 字符占的列数：全角 2、半角 1、组合字符与控制字符 0。
pub fn char_width(ch: char) -> usize {
    if ch.is_control() {
        return 0;
    }
    ch.width().unwrap_or(0)
}

/// 列上一段同一样式的文本。
///
/// `wide` 为真时 `text` 恰好一个全角字符（占 2 列）；否则每个字符占 1 列。
/// 组合字符跟随所属字符，含在 `text` 里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// 起始列（0 起）。
    pub col: usize,
    /// 文本。
    pub text: String,
    /// 样式。
    pub style: Style,
    /// 是否恰好一个全角字符。
    pub wide: bool,
}

impl Span {
    /// 占的列数（组合字符不占列）。
    pub fn cols(&self) -> usize {
        if self.wide {
            2
        } else {
            self.text.chars().map(char_width).sum()
        }
    }
}

/// 一行文本（若干 [`Span`]）。
pub type Line = Vec<Span>;

/// 把一行拼成纯文本（段之间的空隙补空格；全角字符后面留出它占的第二列）。
pub fn text_of(line: &Line) -> String {
    let mut out = String::new();
    let mut col = 0;
    for span in line {
        while col < span.col {
            out.push(' ');
            col += 1;
        }
        out.push_str(&span.text);
        col += span.cols();
    }
    out
}

/// 取一行文本里落在列区间 `[start, end)` 的部分（按显示列裁剪，全角字符只算在起始列）。
pub fn clip_cols(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    let mut included = false;
    for ch in text.chars() {
        let width = char_width(ch);
        if width > 0 && col >= end {
            break;
        }
        if width == 0 {
            // 组合字符属于前一个字符：前一个在选区里它才在（选区边界上的组合字符也要带上）
            if included {
                out.push(ch);
            }
            continue;
        }
        included = col >= start && col < end;
        if included {
            out.push(ch);
        }
        col += width;
    }
    out
}

/// 默认样式（全属性关闭）。
const DEFAULT_STYLE: Style = Style {
    fg: None,
    bg: None,
    bold: false,
    dim: false,
    italic: false,
    underline: false,
    strike: false,
    inverse: false,
};

impl Default for Style {
    fn default() -> Self {
        DEFAULT_STYLE
    }
}

/// 单元格宽度类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellWidth {
    /// 半角，占 1 列。
    Narrow,
    /// 全角，占 2 列（本格 + 下一格的 [`CellWidth::Continuation`]）。
    Wide,
    /// 全角字符的第二列：自身没有字符。
    Continuation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    ch: char,
    style: Style,
    width: CellWidth,
    /// 跟随本字符的组合字符（零宽）。
    marks: String,
}

fn blank(style: Style) -> Cell {
    Cell {
        ch: ' ',
        style,
        width: CellWidth::Narrow,
        marks: String::new(),
    }
}

impl Default for Cell {
    fn default() -> Self {
        blank(DEFAULT_STYLE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEsc,
    Charset,
}

/// 一行的渲染数据：若干段 + 可选的光标位置（列、是否落在全角字符上）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 文本段。
    pub spans: Line,
    /// 光标所在的列与它是否落在全角字符上；这一行没有光标时为 `None`。
    pub cursor: Option<(usize, bool)>,
}

/// 切到备用屏幕时存起来的主屏状态。
struct MainScreen {
    scrollback: Vec<Line>,
    screen: Vec<Vec<Cell>>,
    row: usize,
    col: usize,
    saved: (usize, usize, Style),
    top: usize,
    bottom: usize,
}

/// 终端屏幕模型：`rows × cols` 主屏 + 滚动历史。
pub struct VtScreen {
    cols: usize,
    rows: usize,
    scrollback: Vec<Line>,
    /// 历史头部累计被裁掉的行数：缓冲区行号 + 它 = 不会因裁剪而复用的绝对行号。
    dropped: usize,
    screen: Vec<Vec<Cell>>,
    row: usize,
    col: usize,
    saved: (usize, usize, Style),
    top: usize,
    bottom: usize,
    style: Style,
    state: State,
    csi: String,
    /// 跨块残留的不完整 UTF-8 尾部。
    pending: Vec<u8>,
    /// 待发回远端的应答（DA / DSR 等）。
    responses: Vec<u8>,
    cursor_visible: bool,
    bracketed_paste: bool,
    application_cursor_keys: bool,
    /// 备用屏幕（`?1049h`）：期间不记历史，退出后主屏原样回来。
    alt: Option<MainScreen>,
}

impl VtScreen {
    /// 创建全空屏幕（尺寸至少 1×1）。
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            scrollback: Vec::new(),
            dropped: 0,
            screen: vec![vec![Cell::default(); cols]; rows],
            row: 0,
            col: 0,
            saved: (0, 0, Style::default()),
            top: 0,
            bottom: rows - 1,
            style: Style::default(),
            state: State::Ground,
            csi: String::new(),
            pending: Vec::new(),
            responses: Vec::new(),
            cursor_visible: true,
            bracketed_paste: false,
            application_cursor_keys: false,
            alt: None,
        }
    }

    /// 屏幕尺寸（列，行）。
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// 光标位置（行，列），0 起。
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// 光标是否显示（`?25l` 隐藏）。
    #[cfg(test)]
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// 远端是否开启了括号粘贴模式（粘贴的文本要包在 `ESC[200~ … ESC[201~` 里）。
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// 远端是否要求应用光标键模式（方向键发 `ESC O A` 而不是 `ESC [ A`）。
    pub fn application_cursor_keys(&self) -> bool {
        self.application_cursor_keys
    }

    /// 取走待发回远端的应答（查询应答），由输入侧写回远端。
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// 总行数（滚动历史 + 主屏）。
    pub fn line_count(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// 缓冲区第 0 行对应的绝对行号（历史头部每被裁掉一行加一）。
    pub fn first_line(&self) -> usize {
        self.dropped
    }

    /// 是否停在备用屏幕上（此时没有可翻看的历史）。
    pub fn is_alternate(&self) -> bool {
        self.alt.is_some()
    }

    /// 第 `index` 行（0 起，先是滚动历史再是主屏）的纯文本。
    #[cfg(test)]
    pub fn line_text(&self, index: usize) -> String {
        if index < self.scrollback.len() {
            return text_of(&self.scrollback[index]);
        }
        self.screen
            .get(index - self.scrollback.len())
            .map_or_else(String::new, |row| text_of(&collapse(row)))
    }

    /// 改变屏幕尺寸：每行截断 / 补空；行数减少时顶部几行进历史，增多时底部补空行。
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        for row in &mut self.screen {
            row.truncate(cols);
            row.resize_with(cols, || blank(DEFAULT_STYLE));
            Self::repair_wide(row);
        }
        while self.screen.len() > rows {
            let dropped = self.screen.remove(0);
            if self.alt.is_none() {
                self.scrollback.push(collapse(&dropped));
            }
        }
        while self.screen.len() < rows {
            self.screen.push(vec![Cell::default(); cols]);
        }
        let excess = self.scrollback.len().saturating_sub(SCROLLBACK_CAP);
        self.dropped += excess;
        self.scrollback.drain(..excess);
        self.cols = cols;
        self.rows = rows;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        self.saved.0 = self.saved.0.min(rows - 1);
        self.saved.1 = self.saved.1.min(cols - 1);
        self.top = 0;
        self.bottom = rows - 1;
    }

    /// 全量复位（清屏 + 清历史 + 光标/样式/滚动区归位）。
    pub fn full_reset(&mut self) {
        self.cols = self.cols.max(1);
        // 旧内容的绝对行号全部作废：不会被新内容复用
        self.dropped += self.line_count();
        self.scrollback.clear();
        self.screen = vec![vec![Cell::default(); self.cols]; self.rows];
        self.row = 0;
        self.col = 0;
        self.saved = (0, 0, Style::default());
        self.top = 0;
        self.bottom = self.rows - 1;
        self.style = Style::default();
        self.state = State::Ground;
        self.csi.clear();
        self.pending.clear();
        self.responses.clear();
        self.cursor_visible = true;
        self.bracketed_paste = false;
        self.application_cursor_keys = false;
        self.alt = None;
    }

    /// 追加一段输出（可在任意位置切分：跨块的 UTF-8 与转义序列均正确拼接）。
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut rest: &[u8] = &data;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.feed(text);
                    break;
                }
                Err(err) => {
                    let (head, tail) = rest.split_at(err.valid_up_to());
                    self.feed(std::str::from_utf8(head).expect("合法 UTF-8 前缀"));
                    match err.error_len() {
                        // 非法字节：替换为 U+FFFD 后继续
                        Some(len) => {
                            self.feed("\u{FFFD}");
                            rest = &tail[len.min(tail.len())..];
                        }
                        // 结尾是不完整的 UTF-8：留待下一块
                        None => {
                            self.pending = tail.to_vec();
                            break;
                        }
                    }
                }
            }
        }
    }

    /// 追加一段已解码的文本。
    pub fn feed(&mut self, text: &str) {
        for ch in text.chars() {
            self.step(ch);
        }
    }

    /// 最近 `max_lines` 行（滚动历史尾部 + 主屏）的纯文本段。
    #[cfg(test)]
    pub fn render_lines(&self, max_lines: usize) -> Vec<Line> {
        self.render_rows(max_lines)
            .into_iter()
            .map(|row| row.spans)
            .collect()
    }

    /// 最近 `max_lines` 行的渲染数据（滚动历史尾部 + 主屏）；光标行带光标位置。
    #[cfg(test)]
    pub fn render_rows(&self, max_lines: usize) -> Vec<Row> {
        let skip = self.line_count().saturating_sub(max_lines);
        (skip..self.line_count())
            .map(|index| self.render_row(index).expect("行号来自 line_count 之内"))
            .collect()
    }

    /// 第 `index` 行（0 起，先是滚动历史再是主屏）的渲染数据；光标行带光标位置
    /// （列号是所在单元格的起始列：落在全角字符上时是它的第一列）。
    pub fn render_row(&self, index: usize) -> Option<Row> {
        let history = self.scrollback.len();
        let spans = if index < history {
            self.scrollback.get(index).cloned()?
        } else {
            collapse(self.screen.get(index - history)?)
        };
        if index != history + self.row || !self.cursor_visible {
            return Some(Row {
                spans,
                cursor: None,
            });
        }
        let cursor_cell = self.screen[self.row].get(self.col).map(|cell| cell.width);
        let cursor_wide = matches!(cursor_cell, Some(CellWidth::Wide | CellWidth::Continuation));
        // 光标停在全角字符的任意一半上都算落在这个字符上：列号归到它的第一列
        let col = if cursor_cell == Some(CellWidth::Continuation) {
            self.col - 1
        } else {
            self.col
        };
        Some(Row {
            cursor: Some((col, cursor_wide)),
            spans,
        })
    }

    fn step(&mut self, ch: char) {
        match self.state {
            State::Ground => match ch {
                '\x1b' => self.state = State::Escape,
                '\r' => self.col = 0,
                '\n' | '\x0b' | '\x0c' => self.line_feed(),
                '\u{8}' => self.backspace(),
                '\t' => self.col = ((self.col / 8 + 1) * 8).min(self.cols - 1),
                c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {}
                c => self.put_char(c),
            },
            State::Escape => self.escape(ch),
            State::Csi => self.csi_step(ch),
            State::Osc => match ch {
                '\x07' => self.state = State::Ground,
                '\x1b' => self.state = State::OscEsc,
                _ => {}
            },
            State::OscEsc => match ch {
                '\\' | '\x07' => self.state = State::Ground,
                _ => {
                    self.state = State::Escape;
                    self.escape(ch);
                }
            },
            State::Charset => self.state = State::Ground,
        }
    }

    fn wrap(&mut self) {
        self.col = 0;
        self.line_feed();
    }

    /// 修好一行里被擦写 / 插删切断的全角字符：孤零零的第二列清成空格，缺第二列的全角字符补上。
    fn repair_wide(row: &mut [Cell]) {
        for col in 0..row.len() {
            match row[col].width {
                CellWidth::Continuation => {
                    if col == 0 || row[col - 1].width != CellWidth::Wide {
                        row[col].width = CellWidth::Narrow;
                        row[col].ch = ' ';
                    }
                }
                // 第二列没了（被擦掉或被截断）：整个字符清掉，不留半个字
                CellWidth::Wide
                    if row
                        .get(col + 1)
                        .map_or(true, |next| next.width != CellWidth::Continuation) =>
                {
                    row[col] = blank(row[col].style);
                }
                CellWidth::Wide => {}
                CellWidth::Narrow => {}
            }
        }
    }

    /// 在 `row` 的 `col` 处落笔前，清掉可能被拆开的全角字符的另一半。
    fn break_wide_at(&mut self, row: usize, col: usize) {
        let blank_style = self.blank_style();
        let cells = &mut self.screen[row];
        match cells[col].width {
            CellWidth::Wide => {
                if let Some(next) = cells.get_mut(col + 1) {
                    if next.width == CellWidth::Continuation {
                        *next = blank(blank_style);
                    }
                }
                cells[col] = blank(blank_style);
            }
            CellWidth::Continuation => {
                cells[col] = blank(blank_style);
                if col > 0 {
                    cells[col - 1] = blank(blank_style);
                }
            }
            CellWidth::Narrow => {}
        }
    }

    fn put_char(&mut self, ch: char) {
        let width = char_width(ch);
        if width == 0 {
            self.attach_mark(ch);
            return;
        }
        if self.col + width > self.cols {
            self.wrap();
        }
        self.break_wide_at(self.row, self.col);
        if width == 2 {
            self.break_wide_at(self.row, self.col + 1);
        }
        self.screen[self.row][self.col] = Cell {
            ch,
            style: self.style,
            width: if width == 2 {
                CellWidth::Wide
            } else {
                CellWidth::Narrow
            },
            marks: String::new(),
        };
        if width == 2 {
            self.screen[self.row][self.col + 1] = Cell {
                ch: ' ',
                style: self.style,
                width: CellWidth::Continuation,
                marks: String::new(),
            };
        }
        self.col += width;
    }

    /// 零宽字符跟随前一个有字符的格（本行前一格；行首则是上一行最后一格）。
    fn attach_mark(&mut self, ch: char) {
        if self.col > 0 {
            let at = self.mark_owner(self.row, self.col - 1);
            self.screen[self.row][at].marks.push(ch);
        } else if self.row > 0 {
            let last = self.cols - 1;
            let at = if self.screen[self.row - 1][last].width == CellWidth::Continuation {
                last - 1
            } else {
                last
            };
            self.screen[self.row - 1][at].marks.push(ch);
        }
    }

    /// 第 `col` 格上的字符实际所在的格（全角字符的第二列归第一列）。
    fn mark_owner(&self, row: usize, col: usize) -> usize {
        if col > 0 && self.screen[row][col].width == CellWidth::Continuation {
            col - 1
        } else {
            col
        }
    }

    fn backspace(&mut self) {
        if self.col == 0 {
            return;
        }
        if self.screen[self.row][self.col - 1].width == CellWidth::Continuation {
            self.col -= 2;
        } else {
            self.col -= 1;
        }
    }

    fn line_feed(&mut self) {
        if self.row == self.bottom {
            self.scroll_up(1);
        } else if self.row < self.rows - 1 {
            self.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.row == self.top {
            self.scroll_down(1);
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    fn cursor_up(&mut self, n: usize) {
        let limit = if self.row >= self.top { self.top } else { 0 };
        self.row = self.row.saturating_sub(n).max(limit);
    }

    fn cursor_down(&mut self, n: usize) {
        let limit = if self.row <= self.bottom {
            self.bottom
        } else {
            self.rows - 1
        };
        self.row = (self.row + n).min(limit);
    }

    fn blank_style(&self) -> Style {
        Style {
            bg: self.style.bg,
            ..Style::default()
        }
    }

    fn blank_row(&self) -> Vec<Cell> {
        let style = self.blank_style();
        vec![blank(style); self.cols]
    }

    /// 区内上滚：顶部滚出主屏顶的行进入历史（备用屏幕不记历史）。
    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        let history: Vec<Line> = self
            .screen
            .drain(self.top..self.top + n)
            .map(|row| collapse(&row))
            .collect();
        if self.top == 0 && self.alt.is_none() {
            self.scrollback.extend(history);
            let excess = self.scrollback.len().saturating_sub(SCROLLBACK_CAP);
            self.dropped += excess;
            self.scrollback.drain(..excess);
        }
        let at = self.bottom + 1 - n;
        let blanks: Vec<Vec<Cell>> = (0..n).map(|_| self.blank_row()).collect();
        self.screen.splice(at..at, blanks);
    }

    /// 区内下滚：底部滚出主屏的行丢弃。
    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        let cut = self.bottom + 1 - n;
        self.screen.drain(cut..cut + n);
        let blanks: Vec<Vec<Cell>> = (0..n).map(|_| self.blank_row()).collect();
        self.screen.splice(self.top..self.top, blanks);
    }

    fn erase_cells(&mut self, row: usize, start: usize, end: usize) {
        if start >= self.cols {
            return;
        }
        let style = self.blank_style();
        let end = end.min(self.cols);
        for cell in &mut self.screen[row][start..end] {
            *cell = blank(style);
        }
        Self::repair_wide(&mut self.screen[row]);
    }

    fn erase_display(&mut self, mode: u32) {
        match mode {
            0 => {
                self.erase_cells(self.row, self.col, self.cols);
                self.fill_blank(self.row + 1, self.rows);
            }
            1 => {
                self.fill_blank(0, self.row);
                self.erase_cells(self.row, 0, self.col + 1);
            }
            2 | 3 => {
                self.fill_blank(0, self.rows);
                if mode == 3 {
                    self.dropped += self.scrollback.len();
                    self.scrollback.clear();
                }
            }
            _ => {}
        }
    }

    fn fill_blank(&mut self, from: usize, to: usize) {
        let style = self.blank_style();
        for row in &mut self.screen[from..to] {
            row.fill(blank(style));
            Self::repair_wide(row);
        }
    }

    fn erase_line(&mut self, mode: u32) {
        match mode {
            0 => self.erase_cells(self.row, self.col, self.cols),
            1 => self.erase_cells(self.row, 0, self.col + 1),
            2 => self.erase_cells(self.row, 0, self.cols),
            _ => {}
        }
    }

    fn insert_lines(&mut self, n: usize) {
        if self.row < self.top || self.row > self.bottom {
            return;
        }
        let n = n.min(self.bottom - self.row + 1);
        let cut = self.bottom + 1 - n;
        self.screen.drain(cut..cut + n);
        let blanks: Vec<Vec<Cell>> = (0..n).map(|_| self.blank_row()).collect();
        self.screen.splice(self.row..self.row, blanks);
    }

    fn delete_lines(&mut self, n: usize) {
        if self.row < self.top || self.row > self.bottom {
            return;
        }
        let n = n.min(self.bottom - self.row + 1);
        self.screen.drain(self.row..self.row + n);
        for _ in 0..n {
            self.screen.push(self.blank_row());
        }
    }

    fn escape(&mut self, ch: char) {
        self.state = State::Ground;
        match ch {
            '[' => {
                self.csi.clear();
                self.state = State::Csi;
            }
            ']' => self.state = State::Osc,
            'P' | 'X' | '^' | '_' => self.state = State::Osc,
            '(' | ')' | '*' | '+' => self.state = State::Charset,
            '7' => self.saved = (self.row, self.col, self.style),
            '8' => self.restore_cursor(),
            'D' => self.line_feed(),
            'M' => self.reverse_index(),
            'E' => {
                self.col = 0;
                self.line_feed();
            }
            'c' => self.full_reset(),
            _ => {}
        }
    }

    fn restore_cursor(&mut self) {
        self.row = self.saved.0.min(self.rows - 1);
        self.col = self.saved.1.min(self.cols - 1);
        self.style = self.saved.2;
    }

    fn csi_step(&mut self, ch: char) {
        match ch {
            '\x1b' => self.state = State::Escape,
            '\x18' | '\x1a' => self.state = State::Ground,
            '0'..='?' | ' '..='/' | ':' if self.csi.len() < CSI_CAP => self.csi.push(ch),
            '@'..='~' => {
                self.dispatch(ch);
                self.state = State::Ground;
            }
            _ => {}
        }
    }

    /// 把 CSI 参数串拆成「参数 → 子参数」：`38:5:196;1` → `[[38,5,196],[1]]`。
    fn params(&self) -> Vec<Vec<u32>> {
        self.csi
            .trim_start_matches(['<', '=', '>', '?'])
            .split(';')
            .map(|group| {
                group
                    .split(':')
                    .map(|part| {
                        part.chars()
                            .take_while(char::is_ascii_digit)
                            .collect::<String>()
                            .parse()
                            .unwrap_or(0)
                    })
                    .collect()
            })
            .collect()
    }

    fn dispatch(&mut self, final_byte: char) {
        // 私有序列（`?25l` 等）与带中间字节的一律只做模式跟踪 / 应答，不当普通参数用
        let private = self.csi.starts_with(['<', '=', '>', '?']);
        let intermediate = self.csi.chars().any(|c| matches!(c, ' '..='/'));
        if intermediate {
            return;
        }
        if private {
            self.dispatch_private(final_byte);
            return;
        }
        let groups = self.params();
        let params: Vec<u32> = groups.iter().map(|group| group[0]).collect();
        // 计数类参数：缺省或 0 均按 1
        let n = |index: usize| params.get(index).copied().unwrap_or(0).max(1) as usize;
        let first = params.first().copied().unwrap_or(0);
        match final_byte {
            'm' => self.sgr(&groups),
            'A' => self.cursor_up(n(0)),
            'B' => self.cursor_down(n(0)),
            'C' => self.col = (self.col + n(0)).min(self.cols - 1),
            'D' => self.col = self.col.saturating_sub(n(0)),
            'E' => {
                self.cursor_down(n(0));
                self.col = 0;
            }
            'F' => {
                self.cursor_up(n(0));
                self.col = 0;
            }
            'G' | '`' => self.col = (n(0) - 1).min(self.cols - 1),
            'd' => self.row = (n(0) - 1).min(self.rows - 1),
            'H' | 'f' => {
                self.row = (n(0) - 1).min(self.rows - 1);
                self.col = (n(1) - 1).min(self.cols - 1);
            }
            'J' => self.erase_display(first),
            'K' => self.erase_line(first),
            'L' => self.insert_lines(n(0)),
            'M' => self.delete_lines(n(0)),
            '@' => {
                let count = n(0).min(self.cols - self.col);
                let inserted = vec![blank(self.blank_style()); count];
                self.screen[self.row].splice(self.col..self.col, inserted);
                self.screen[self.row].truncate(self.cols);
                let row = self.row;
                Self::repair_wide(&mut self.screen[row]);
            }
            'P' => {
                let count = n(0).min(self.cols - self.col);
                let filler = vec![blank(self.blank_style()); count];
                self.screen[self.row].drain(self.col..self.col + count);
                self.screen[self.row].extend(filler);
                let row = self.row;
                Self::repair_wide(&mut self.screen[row]);
            }
            'X' => self.erase_cells(self.row, self.col, self.col + n(0)),
            'S' => self.scroll_up(n(0)),
            'T' => self.scroll_down(n(0)),
            'r' => self.set_margins(first, params.get(1).copied()),
            's' => self.saved = (self.row, self.col, self.style),
            'u' => self.restore_cursor(),
            // 状态查询：OK / 光标位置（1 起）
            'n' if first == 5 => self.responses.extend_from_slice(b"\x1b[0n"),
            'n' if first == 6 => self
                .responses
                .extend_from_slice(format!("\x1b[{};{}R", self.row + 1, self.col + 1).as_bytes()),
            // 主设备属性：VT100 级
            'c' => self.responses.extend_from_slice(b"\x1b[?1;2c"),
            _ => {}
        }
    }

    /// 私有序列：DEC 模式开关与次设备属性查询。
    fn dispatch_private(&mut self, final_byte: char) {
        if self.csi.starts_with('>') {
            if final_byte == 'c' {
                // 次设备属性：VT100 / xterm 版本 95 / 固件 0
                self.responses.extend_from_slice(b"\x1b[>0;95;0c");
            }
            return;
        }
        if !self.csi.starts_with('?') {
            return;
        }
        match final_byte {
            'c' => self.responses.extend_from_slice(b"\x1b[?1;2c"),
            'h' | 'l' => {
                let on = final_byte == 'h';
                for mode in self.params().iter().map(|group| group[0]) {
                    match mode {
                        1 => self.application_cursor_keys = on,
                        25 => self.cursor_visible = on,
                        2004 => self.bracketed_paste = on,
                        47 | 1047 | 1049 => self.set_alt_screen(on),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn set_alt_screen(&mut self, on: bool) {
        if on == self.alt.is_some() {
            return;
        }
        if on {
            let saved = self.saved;
            self.alt = Some(MainScreen {
                scrollback: std::mem::take(&mut self.scrollback),
                screen: std::mem::replace(
                    &mut self.screen,
                    vec![vec![Cell::default(); self.cols]; self.rows],
                ),
                row: self.row,
                col: self.col,
                saved,
                top: self.top,
                bottom: self.bottom,
            });
            self.row = 0;
            self.col = 0;
            self.top = 0;
            self.bottom = self.rows - 1;
            return;
        }
        if let Some(main) = self.alt.take() {
            let MainScreen {
                scrollback,
                screen,
                row,
                col,
                saved,
                top,
                bottom,
            } = main;
            self.scrollback = scrollback;
            self.screen = screen;
            self.row = row.min(self.rows - 1);
            self.col = col.min(self.cols - 1);
            self.top = top.min(self.rows - 1);
            self.bottom = bottom.min(self.rows - 1);
            self.saved = saved;
        }
    }

    fn set_margins(&mut self, top: u32, bottom: Option<u32>) {
        let top = top.max(1) as usize - 1;
        let bottom = bottom.map_or(self.rows, |b| b as usize).saturating_sub(1);
        if top < bottom && bottom < self.rows {
            self.top = top;
            self.bottom = bottom;
            self.row = 0;
            self.col = 0;
        }
    }

    fn sgr(&mut self, groups: &[Vec<u32>]) {
        let mut i = 0;
        while i < groups.len() {
            let p = groups[i][0];
            match p {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => {
                    self.style.underline = groups.get(i).map_or(true, |group| {
                        group.len() < 2 || !matches!(group[1], 0 | 24 | 27)
                    })
                }
                5 | 6 | 8 => {} // 闪烁 / 隐藏：不影响静态显示
                7 => self.style.inverse = true,
                9 => self.style.strike = true,
                21 => self.style.underline = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                29 => self.style.strike = false,
                30..=37 => self.style.fg = Some(Color::Indexed((p - 30) as u8)),
                39 => self.style.fg = None,
                40..=47 => self.style.bg = Some(Color::Indexed((p - 40) as u8)),
                49 => self.style.bg = None,
                90..=97 => self.style.fg = Some(Color::Indexed((p - 82) as u8)),
                100..=107 => self.style.bg = Some(Color::Indexed((p - 92) as u8)),
                38 | 48 => {
                    // 冒号形式（`38:5:n` / `38:2:r:g:b`）子参数都在本组里；
                    // 分号形式的后续参数最多看 4 个（`2 r g b`）
                    let (color, skip) = if groups[i].len() > 1 {
                        (Self::extended_color(&groups[i][1..]), 0)
                    } else {
                        let tail: Vec<u32> = groups
                            .iter()
                            .skip(i + 1)
                            .take(4)
                            .map(|group| group[0])
                            .collect();
                        let skip = Self::extended_color_len(&tail);
                        (Self::extended_color(&tail), skip)
                    };
                    if let Some(color) = color {
                        if p == 38 {
                            self.style.fg = Some(color);
                        } else {
                            self.style.bg = Some(color);
                        }
                    }
                    i += skip;
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// 扩展颜色的参数体：`5 n`（256 色）或 `2 r g b`（真彩色）。
    /// 扩展颜色的参数体：`5 n`（256 色）或 `2 r g b`（真彩色）。
    fn extended_color(params: &[u32]) -> Option<Color> {
        match params {
            [5, n, ..] => Some(Color::Indexed(*n as u8)),
            [2, r, g, b] => Some(Color::Rgb(Rgb(*r as u8, *g as u8, *b as u8))),
            // 真彩色多一个色彩空间占位（ITU T.416，常为空 → 0）
            [2, _, r, g, b] => Some(Color::Rgb(Rgb(*r as u8, *g as u8, *b as u8))),
            _ => None,
        }
    }

    /// 分号形式下扩展颜色占掉的后续参数个数。
    fn extended_color_len(params: &[u32]) -> usize {
        match params {
            [5, ..] => 2,
            [2, ..] => 4,
            _ => 0,
        }
    }
}

/// 把一行单元格合并为若干 [`Span`]（全角字符各自成段），并裁掉行尾无底色的空白。
fn collapse(cells: &[Cell]) -> Line {
    let last = cells.iter().rposition(|cell| {
        cell.ch != ' '
            || cell.width != CellWidth::Narrow
            || !cell.marks.is_empty()
            || cell.style.bg.is_some()
            || cell.style.inverse
    });
    let Some(last) = last else {
        return Vec::new();
    };
    let mut line: Line = Vec::new();
    let mut text = String::new();
    let mut start = 0;
    let mut current: Option<Style> = None;
    let mut col = 0;
    let flush = |line: &mut Line, text: &mut String, current: &mut Option<Style>, start: usize| {
        if let Some(style) = current.take() {
            if !text.is_empty() {
                line.push(Span {
                    col: start,
                    text: std::mem::take(text),
                    style,
                    wide: false,
                });
            }
        }
    };
    for cell in &cells[..=last] {
        if cell.width == CellWidth::Continuation {
            // 全角字符的第二列：没有字符，只占位
            col += 1;
            continue;
        }
        // 全角字符各自成段：渲染端按列定位，它后面的内容才不会跟着字体回退的宽度漂移
        let split = cell.width == CellWidth::Wide || current != Some(cell.style);
        if split {
            flush(&mut line, &mut text, &mut current, start);
            start = col;
        }
        if cell.width == CellWidth::Wide {
            line.push(Span {
                col,
                text: format!("{}{}", cell.ch, cell.marks),
                style: cell.style,
                wide: true,
            });
            current = None;
            // 只先计本格：紧随的 Continuation 格再计第二列
            col += 1;
            continue;
        }
        current = Some(cell.style);
        text.push(cell.ch);
        text.push_str(&cell.marks);
        col += 1;
    }
    flush(&mut line, &mut text, &mut current, start);
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line) -> String {
        line.iter().map(|span| span.text.as_str()).collect()
    }

    fn screen(text: &str) -> VtScreen {
        let mut vt = VtScreen::new(20, 4);
        vt.feed(text);
        vt
    }

    fn render(vt: &VtScreen) -> Vec<String> {
        vt.render_lines(usize::MAX).iter().map(plain).collect()
    }

    fn cell_of(ch: char) -> Cell {
        Cell {
            ch,
            style: DEFAULT_STYLE,
            width: if char_width(ch) == 2 {
                CellWidth::Wide
            } else {
                CellWidth::Narrow
            },
            marks: String::new(),
        }
    }

    fn cells_of(text: &str) -> Vec<Cell> {
        let mut cells = vec![Cell::default(); 20];
        for (cell, ch) in cells.iter_mut().zip(text.chars()) {
            *cell = cell_of(ch);
        }
        cells
    }

    fn spans_of(vt: &VtScreen, row: usize) -> Line {
        vt.render_rows(usize::MAX)[row].spans.clone()
    }

    #[test]
    fn plain_text_and_cursor_return() {
        assert_eq!(
            render(&screen("hello\r\nworld")),
            ["hello", "world", "", ""]
        );
        // 进度条式回车覆盖
        assert_eq!(render(&screen("10%\r20%"))[0], "20%");
        assert_eq!(render(&screen("abc\u{8}X"))[0], "abX");
        assert_eq!(render(&screen("a\tb"))[0], "a       b");
    }

    #[test]
    fn sgr_colors_split_into_spans() {
        let vt = screen("\x1b[31mred\x1b[0m plain");
        let line = spans_of(&vt, 0);
        assert_eq!(line.len(), 2);
        assert_eq!(line[0].text, "red");
        assert_eq!(line[0].col, 0);
        assert_eq!(line[0].style.fg, Some(Color::Indexed(1)));
        assert_eq!(
            line[0].style.resolve_pair(DEFAULT_FG, DEFAULT_BG).0,
            palette(1)
        );
        assert_eq!(line[1].text, " plain");
        assert_eq!(line[1].col, 3);
        assert_eq!(line[1].style, Style::default());
    }

    #[test]
    fn extended_colors_bold_inverse_dim() {
        let vt = screen("\x1b[1;31mA\x1b[38;5;196mB\x1b[48;2;1;2;3mC\x1b[7mD");
        let line = &spans_of(&vt, 0);
        assert_eq!(plain(line), "ABCD");
        // 粗体 + 标准红 → 亮红
        assert_eq!(
            line[0].style.resolve_pair(DEFAULT_FG, DEFAULT_BG).0,
            palette(9)
        );
        assert_eq!(line[1].style.fg, Some(Color::Indexed(196)));
        assert_eq!(
            line[2].style.bg,
            Some(Color::Rgb(Rgb(1, 2, 3))),
            "真彩色背景"
        );
        // 前景背景成对解析：反显交换两者（D 沿用了 C 的真彩色底）
        assert_eq!(
            line[3].style.resolve_pair(DEFAULT_FG, DEFAULT_BG),
            (Rgb(1, 2, 3), palette(196))
        );
        assert_eq!(
            line[2].style.resolve_pair(DEFAULT_FG, DEFAULT_BG),
            (palette(196), Rgb(1, 2, 3))
        );
        let plain_inverse = screen("\x1b[7mx");
        let style = plain_inverse
            .render_rows(usize::MAX)
            .remove(0)
            .spans
            .remove(0)
            .style;
        assert_eq!(
            style.resolve_pair(DEFAULT_FG, DEFAULT_BG),
            (DEFAULT_BG, DEFAULT_FG)
        );
        // 暗淡：前景向背景折半
        let dim = screen("\x1b[2;37mx");
        let style = dim.render_rows(usize::MAX).remove(0).spans.remove(0).style;
        assert_eq!(
            style.resolve_pair(DEFAULT_FG, DEFAULT_BG).0,
            mix(palette(7), DEFAULT_BG)
        );
    }

    #[test]
    fn sgr_additions_strike_colon_colors_and_reset_rules() {
        // 删除线、冒号形式的 256 色 / 真彩色、21 = 下划线、22 = 取消粗体与暗淡
        let vt = screen("\x1b[9m\x1b[38:5:196mA\x1b[0m\x1b[48:2:1:2:3mB\x1b[1;2;21m\x1b[22mC");
        let rows = vt.render_rows(usize::MAX);
        let line = &rows[0].spans;
        assert!(line[0].style.strike);
        assert_eq!(line[0].style.fg, Some(Color::Indexed(196)));
        assert_eq!(line[1].style.bg, Some(Color::Rgb(Rgb(1, 2, 3))));
        // C 沿用了 B 的底色
        assert_eq!(
            line[2].style,
            Style {
                bg: Some(Color::Rgb(Rgb(1, 2, 3))),
                underline: true,
                ..Style::default()
            }
        );
        // 冒号形式之后再跟普通参数也接得上
        let vt = screen("\x1b[38:2:1:2:3;4mA");
        let style = vt.render_rows(usize::MAX)[0].spans[0].style;
        assert_eq!(style.fg, Some(Color::Rgb(Rgb(1, 2, 3))));
        assert!(style.underline);
    }

    #[test]
    fn erase_display_and_line() {
        // 擦除整行不移动光标：后续输出从原光标列继续
        assert_eq!(render(&screen("hello\x1b[2Kworld"))[0], "     world");
        assert_eq!(render(&screen("hello\x1b[2K\rworld"))[0], "world");
        assert_eq!(render(&screen("hello\x1b[1Kx"))[0], "     x");
        let vt = screen("aaa\r\nbbb\x1b[2J\x1b[HX");
        assert_eq!(render(&vt), ["X", "", "", ""]);
    }

    #[test]
    fn cursor_addressing_overwrites() {
        let vt = screen("\x1b[2;3HX");
        assert_eq!(render(&vt), ["", "  X", "", ""]);
    }

    #[test]
    fn scrolling_pushes_history() {
        let vt = screen("1\r\n2\r\n3\r\n4\r\n5\r\n6");
        assert_eq!(
            vt.scrollback,
            [cells_of("1"), cells_of("2")].map(|cells| collapse(&cells))
        );
        assert_eq!(render(&vt), ["1", "2", "3", "4", "5", "6"]);
        // 历史有上限
        let mut long = VtScreen::new(8, 2);
        for i in 0..(SCROLLBACK_CAP + 10) {
            long.feed(&format!("{i}\r\n"));
        }
        assert_eq!(long.scrollback.len(), SCROLLBACK_CAP);
    }

    #[test]
    fn scroll_region_confines_scrolling() {
        let mut vt = VtScreen::new(4, 4);
        vt.feed("a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[2;1H\n\n");
        assert_eq!(render(&vt), ["a", "c", "", "d"]);
    }

    #[test]
    fn insert_and_delete() {
        assert_eq!(render(&screen("abcdef\x1b[1;1H\x1b[2P"))[0], "cdef");
        assert_eq!(
            render(&screen("abcdef\x1b[1;1H\x1b[2@"))[0],
            "  abcdef".trim_end()
        );
        assert_eq!(render(&screen("abcdef\x1b[1;1H\x1b[3X"))[0], "   def");
    }

    #[test]
    fn osc_and_unknown_sequences_are_discarded() {
        assert_eq!(render(&screen("\x1b]0;窗口标题\x07ok"))[0], "ok");
        assert_eq!(render(&screen("\x1b]0;title\x1b\\ok"))[0], "ok");
        assert_eq!(render(&screen("\x1b(B\x1b[5nvisible"))[0], "visible");
        // 设备控制串 / 注释串整体丢弃
        assert_eq!(render(&screen("\x1bP+q544e\x1b\\ok"))[0], "ok");
    }

    #[test]
    fn feed_bytes_joins_split_sequences() {
        let mut vt = VtScreen::new(20, 2);
        // UTF-8 多字节字符跨块
        let bytes = "中x".as_bytes();
        vt.feed_bytes(&bytes[..2]);
        vt.feed_bytes(&bytes[2..]);
        // 转义序列跨块
        vt.feed_bytes(b"\x1b[3");
        vt.feed_bytes(b"1mred");
        let line = vt.render_lines(usize::MAX).remove(0);
        assert_eq!(plain(&line), "中xred");
        let red = line.iter().find(|span| span.text == "red").expect("红色段");
        assert_eq!(red.style.fg, Some(Color::Indexed(1)));
        assert_eq!(
            red.col, 3,
            "全角字符占 2 列，x 在第 3 列，red 从第 3 列后开始"
        );
    }

    #[test]
    fn render_tail_and_wrap() {
        let mut vt = VtScreen::new(4, 2);
        vt.feed("abcdef");
        assert_eq!(render(&vt), ["abcd", "ef"]);
        assert_eq!(render(&vt).len(), 2);
        let lines = vt.render_lines(1);
        assert_eq!(plain(&lines[0]), "ef", "只取最近 1 行");
    }

    #[test]
    fn wide_characters_take_two_columns_and_stay_on_one_row() {
        let mut vt = VtScreen::new(6, 2);
        vt.feed("中文ab");
        let spans = spans_of(&vt, 0);
        assert_eq!(
            spans
                .iter()
                .map(|span| (span.col, span.text.as_str()))
                .collect::<Vec<_>>(),
            [(0, "中"), (2, "文"), (4, "ab")]
        );
        assert!(spans[0].wide && spans[1].wide && !spans[2].wide);
        assert_eq!(vt.cursor(), (0, 6));
        // 行尾只剩一列放不下全角字符：整个字符换到下一行
        let mut vt = VtScreen::new(2, 2);
        vt.feed("a文");
        assert_eq!(vt.cursor(), (1, 2));
        assert_eq!(spans_of(&vt, 0)[0].text, "a");
        assert_eq!(spans_of(&vt, 1)[0].text, "文");
        // 覆盖全角字符的前半格：后半格一起清掉
        let mut vt = VtScreen::new(6, 2);
        vt.feed("中文\x1b[1;1HX");
        assert_eq!(
            spans_of(&vt, 0)
                .iter()
                .map(|span| span.text.as_str())
                .collect::<Vec<_>>(),
            ["X ", "文"]
        );
        // 从后半格落笔同样清掉前半格
        let mut vt = VtScreen::new(6, 2);
        vt.feed("中文\x1b[1;2HY");
        assert_eq!(
            spans_of(&vt, 0)
                .iter()
                .map(|span| span.text.as_str())
                .collect::<Vec<_>>(),
            [" Y", "文"]
        );
        // 退格跨过全角字符
        let mut vt = VtScreen::new(6, 2);
        vt.feed("中\u{8}\u{8}Z");
        assert_eq!(vt.cursor(), (0, 1));
        assert_eq!(spans_of(&vt, 0)[0].text, "Z");
    }

    #[test]
    fn combining_marks_follow_their_base_and_take_no_column() {
        let mut vt = VtScreen::new(20, 2);
        vt.feed("e\u{301}x");
        assert_eq!(vt.cursor(), (0, 2));
        let spans = spans_of(&vt, 0);
        assert_eq!(spans[0].text, "e\u{301}x");
        // 全角字符后面的组合字符也跟随：光标停在它的第二列上，组合字符要归到第一列
        vt.feed("\r中\u{309}");
        let spans = spans_of(&vt, 0);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<Vec<_>>(),
            ["中\u{309}"]
        );
    }

    #[test]
    fn text_helpers_respect_columns() {
        let mut vt = VtScreen::new(10, 2);
        vt.feed("中文ab");
        assert_eq!(vt.line_text(0), "中文ab");
        assert_eq!(text_of(&spans_of(&vt, 0)), "中文ab");
        // 按显示列裁剪：全角字符只算在起始列
        assert_eq!(clip_cols("中文ab", 0, 2), "中");
        assert_eq!(clip_cols("中文ab", 2, 6), "文ab");
        // 全角字符「文」从第 2 列开始，落在 [1, 3) 里，整个保留
        assert_eq!(clip_cols("中文ab", 1, 3), "文");
        assert_eq!(clip_cols("中文ab", 3, 6), "ab");
        assert_eq!(clip_cols("e\u{301}x", 0, 1), "e\u{301}");
    }

    #[test]
    fn alternate_screen_round_trip_without_history() {
        let mut vt = VtScreen::new(10, 3);
        vt.feed("prompt$ \x1b[?1049h");
        assert_eq!(render(&vt), ["", "", ""], "进入备用屏：整屏清空");
        vt.feed("full\x1b[3;1Hscreen\x1b[1;1H");
        assert!(vt.scrollback.is_empty(), "备用屏不记历史");
        vt.feed("\x1b[?1049l");
        // 行尾没有底色的空白在渲染时裁掉
        assert_eq!(render(&vt), ["prompt$", "", ""], "回到主屏：原样回来");
        assert_eq!(vt.cursor(), (0, 8));
        // 备用屏里滚出主屏顶的内容不进历史
        let mut vt = VtScreen::new(10, 2);
        vt.feed("main");
        vt.feed("\x1b[?1049h1\r\n2\r\n3");
        assert!(vt.scrollback.is_empty());
        vt.feed("\x1b[?1049l");
        assert_eq!(vt.line_count(), 2);
    }

    #[test]
    fn dec_modes_are_tracked() {
        let mut vt = VtScreen::new(10, 2);
        assert!(vt.cursor_visible());
        vt.feed("\x1b[?25l");
        assert!(!vt.cursor_visible());
        vt.feed("\x1b[?25h\x1b[?2004h\x1b[?1h");
        assert!(vt.cursor_visible());
        assert!(vt.bracketed_paste());
        assert!(vt.application_cursor_keys());
        // 复位清掉模式
        vt.feed("\x1bc");
        assert!(!vt.bracketed_paste() && !vt.application_cursor_keys());
    }

    #[test]
    fn queries_are_answered() {
        let mut vt = VtScreen::new(10, 4);
        vt.feed("ab\x1b[6n\x1b[5n\x1b[c\x1b[>c\x1b[?c");
        assert_eq!(
            vt.take_responses(),
            b"\x1b[1;3R\x1b[0n\x1b[?1;2c\x1b[>0;95;0c\x1b[?1;2c".to_vec()
        );
        assert!(vt.take_responses().is_empty(), "应答只取一次");
    }

    #[test]
    fn cursor_is_reported_on_its_row_only() {
        let mut vt = VtScreen::new(6, 3);
        vt.feed("中文\x1b[2;1H\x1b[1;2H");
        let rows = vt.render_rows(usize::MAX);
        assert_eq!(
            rows[0].cursor,
            Some((0, true)),
            "光标落在全角字符上：列号归到它的第一列"
        );
        assert_eq!(rows[1].cursor, None);
        assert_eq!(rows[2].cursor, None);
        vt.feed("\x1b[?25l");
        assert!(vt
            .render_rows(usize::MAX)
            .iter()
            .all(|row| row.cursor.is_none()));
    }

    #[test]
    fn resize_keeps_text_and_clamps_the_cursor() {
        let mut vt = VtScreen::new(10, 4);
        vt.feed("hello\x1b[4;10H");
        vt.resize(4, 2);
        assert_eq!(vt.size(), (4, 2));
        assert_eq!(vt.cursor(), (1, 3));
        assert_eq!(vt.line_count(), 4, "少掉的两行进了历史");
        assert_eq!(vt.line_text(0), "hell");
        // 行尾的全角字符被切断：整个字符清掉，不留半个字
        let mut vt = VtScreen::new(4, 2);
        vt.feed("ab中");
        vt.resize(3, 2);
        assert_eq!(
            spans_of(&vt, 0)
                .iter()
                .map(|span| span.text.as_str())
                .collect::<Vec<_>>(),
            ["ab"]
        );
        // 变大：右侧与底部补空
        vt.resize(6, 3);
        assert_eq!(vt.size(), (6, 3));
        assert_eq!(vt.line_text(0), "ab");
    }

    #[test]
    fn erasing_half_of_a_wide_character_clears_the_whole_character() {
        let mut vt = VtScreen::new(8, 2);
        vt.feed("中文\x1b[1;2H\x1b[K");
        assert_eq!(vt.line_text(0), "", "从第二列擦到行尾：中、文都清掉");
        let mut vt = VtScreen::new(8, 2);
        vt.feed("中文\x1b[1;2H\x1b[1X");
        // 只擦「中」的第二列：整个「中」清掉，「文」不受影响（全角字符各自成段）
        assert_eq!(
            spans_of(&vt, 0)
                .iter()
                .map(|span| (span.col, span.text.as_str()))
                .collect::<Vec<_>>(),
            [(0, "  "), (2, "文")]
        );
        // 擦「中」的第一列同理
        let mut vt = VtScreen::new(8, 2);
        vt.feed("中文\x1b[1;1H\x1b[1X");
        assert_eq!(
            spans_of(&vt, 0)
                .iter()
                .map(|span| (span.col, span.text.as_str()))
                .collect::<Vec<_>>(),
            [(0, "  "), (2, "文")]
        );
    }
}

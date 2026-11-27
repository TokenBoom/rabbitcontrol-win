//! VT100/xterm 转义序列解析与屏幕模型（终端工具的渲染数据源）。
//!
//! 纯逻辑、无 GUI 依赖：输入远端终端的输出字节，输出按样式合并的文本行
//! 供 XAML 渲染。
//!
//! # 支持范围
//!
//! - SGR：粗体 / 暗淡 / 斜体 / 下划线 / 反显，前景背景 16 色 / 256 色 / 真彩色；
//! - 光标：上下左右、绝对定位、保存 / 恢复；CR / LF / BS / Tab；
//! - 编辑：擦除显示 / 行 / 字符，插入 / 删除字符 / 行；
//! - 滚动区（DECSTBM）与区内滚动、正向 / 反向换行；
//! - OSC（窗口标题等）整体丢弃。
//!
//! # 已知限制
//!
//! - 每个字符按 1 列计（CJK 全角字符不占两列，宽字符表格对齐会偏）；
//! - 不应答 DSR / DA 等查询（依赖应答的全屏程序可能短暂等待）；
//! - 备用屏幕（`?1049`）不切换，全屏程序直接在主屏上渲染；
//! - 背景色不绘制：渲染端只画前景（见 [`Style::resolve_fg_only`]）。

#![deny(missing_docs)]

/// 默认前景色（与终端面板文字色一致，`#C9D8E8`）。
pub const DEFAULT_FG: Rgb = Rgb(0xC9, 0xD8, 0xE8);
/// 默认背景色（与终端面板背景色一致，`#0D1620`）。
pub const DEFAULT_BG: Rgb = Rgb(0x0D, 0x16, 0x20);
/// 滚动历史行数上限。
pub const SCROLLBACK_CAP: usize = 1000;
/// CSI 缓冲上限（防失控序列撑爆内存）。
const CSI_CAP: usize = 64;

/// RGB 颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `0xRRGGBB`（供 XAML 画刷）。
    pub const fn to_hex(self) -> u32 {
        ((self.0 as u32) << 16) | ((self.1 as u32) << 8) | (self.2 as u32)
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
    /// 背景色；`None` = 默认（不绘制底色）。
    pub bg: Option<Color>,
    /// 粗体（标准 8 色同时提亮为亮色）。
    pub bold: bool,
    /// 暗淡。
    pub dim: bool,
    /// 斜体。
    pub italic: bool,
    /// 下划线。
    pub underline: bool,
    /// 反显（前景背景交换）。
    pub inverse: bool,
}

impl Style {
    /// 仅绘制前景时（渲染端不支持底色）的前景色。
    ///
    /// 反显改为直接使用前景色（未设置则提亮为白色）——若按交换语义取背景色，
    /// 反显文字会变成深色、在深色终端底上不可见。
    pub fn resolve_fg_only(self, default_fg: Rgb, default_bg: Rgb) -> Rgb {
        if self.inverse {
            return self
                .fg
                .map_or(Rgb(255, 255, 255), |c| Self::resolve(c, self.bold));
        }
        let fg = self.fg.map_or(default_fg, |c| Self::resolve(c, self.bold));
        if self.dim {
            mix(fg, default_bg)
        } else {
            fg
        }
    }

    fn resolve(color: Color, bold: bool) -> Rgb {
        match color {
            Color::Rgb(rgb) => rgb,
            Color::Indexed(n) => palette(if bold && n < 8 { n + 8 } else { n }),
        }
    }
}

/// xterm 256 色调色板。
pub fn palette(index: u8) -> Rgb {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    const BASIC: [Rgb; 16] = [
        Rgb(0, 0, 0),
        Rgb(205, 0, 0),
        Rgb(0, 205, 0),
        Rgb(205, 205, 0),
        Rgb(0, 0, 238),
        Rgb(205, 0, 205),
        Rgb(0, 205, 205),
        Rgb(229, 229, 229),
        Rgb(127, 127, 127),
        Rgb(255, 0, 0),
        Rgb(0, 255, 0),
        Rgb(255, 255, 0),
        Rgb(92, 92, 255),
        Rgb(255, 0, 255),
        Rgb(0, 255, 255),
        Rgb(255, 255, 255),
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

fn mix(a: Rgb, b: Rgb) -> Rgb {
    Rgb(
        ((u16::from(a.0) + u16::from(b.0)) / 2) as u8,
        ((u16::from(a.1) + u16::from(b.1)) / 2) as u8,
        ((u16::from(a.2) + u16::from(b.2)) / 2) as u8,
    )
}

/// 同一样式的连续文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// 文本。
    pub text: String,
    /// 样式。
    pub style: Style,
}

/// 一行文本（若干 [`Span`]）。
pub type Line = Vec<Span>;

/// 默认样式（全属性关闭）。
const DEFAULT_STYLE: Style = Style {
    fg: None,
    bg: None,
    bold: false,
    dim: false,
    italic: false,
    underline: false,
    inverse: false,
};

impl Default for Style {
    fn default() -> Self {
        DEFAULT_STYLE
    }
}

/// 空单元格（`char::default()` 是 NUL，必须显式写成空格）。
const BLANK: Cell = Cell {
    ch: ' ',
    style: DEFAULT_STYLE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        BLANK
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

/// 终端屏幕模型：`rows × cols` 主屏 + 滚动历史。
pub struct VtScreen {
    cols: usize,
    rows: usize,
    scrollback: Vec<Line>,
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
        }
    }

    /// 全量复位（清屏 + 清历史 + 光标/样式/滚动区归位）。
    pub fn full_reset(&mut self) {
        self.cols = self.cols.max(1);
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

    /// 最近 `max_lines` 行（滚动历史尾部 + 主屏），供渲染。
    pub fn render_lines(&self, max_lines: usize) -> Vec<Line> {
        let total = self.scrollback.len() + self.rows;
        let skip = total.saturating_sub(max_lines);
        self.scrollback
            .iter()
            .cloned()
            .chain(self.screen.iter().map(|row| collapse(row)))
            .skip(skip)
            .collect()
    }

    fn step(&mut self, ch: char) {
        match self.state {
            State::Ground => match ch {
                '\x1b' => self.state = State::Escape,
                '\r' => self.col = 0,
                '\n' | '\x0b' | '\x0c' => self.line_feed(),
                '\u{8}' => self.col = self.col.saturating_sub(1),
                '\t' => self.col = ((self.col / 8 + 1) * 8).min(self.cols - 1),
                c if (c as u32) < 0x20 => {}
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

    fn put_char(&mut self, ch: char) {
        if self.col >= self.cols {
            self.col = 0;
            self.line_feed();
        }
        self.screen[self.row][self.col] = Cell {
            ch,
            style: self.style,
        };
        self.col += 1;
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
        vec![
            Cell {
                ch: ' ',
                style: self.blank_style(),
            };
            self.cols
        ]
    }

    /// 区内上滚：顶部滚出主屏顶的行进入历史。
    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        let history: Vec<Line> = self
            .screen
            .drain(self.top..self.top + n)
            .map(|row| collapse(&row))
            .collect();
        if self.top == 0 {
            self.scrollback.extend(history);
            let excess = self.scrollback.len().saturating_sub(SCROLLBACK_CAP);
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
        let blank = Cell {
            ch: ' ',
            style: self.blank_style(),
        };
        for cell in &mut self.screen[row][start..end.min(self.cols)] {
            *cell = blank;
        }
    }

    fn erase_display(&mut self, mode: u32) {
        match mode {
            0 => {
                self.erase_cells(self.row, self.col, self.cols);
                let blank = self.blank_style();
                for row in &mut self.screen[self.row + 1..] {
                    row.fill(Cell {
                        ch: ' ',
                        style: blank,
                    });
                }
            }
            1 => {
                let blank = self.blank_style();
                for row in &mut self.screen[..self.row] {
                    row.fill(Cell {
                        ch: ' ',
                        style: blank,
                    });
                }
                self.erase_cells(self.row, 0, self.col + 1);
            }
            2 | 3 => {
                let blank = self.blank_style();
                for row in &mut self.screen {
                    row.fill(Cell {
                        ch: ' ',
                        style: blank,
                    });
                }
                if mode == 3 {
                    self.scrollback.clear();
                }
            }
            _ => {}
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
            '0'..='?' | ' '..='/' if self.csi.len() < CSI_CAP => self.csi.push(ch),
            '@'..='~' => {
                self.dispatch(ch);
                self.state = State::Ground;
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, final_byte: char) {
        // 私有序列（如 `?25l` 光标显隐）与带中间字节的一律忽略
        let private = self.csi.starts_with(['<', '=', '>', '?']);
        let intermediate = self.csi.chars().any(|c| matches!(c, ' '..='/'));
        if private || intermediate {
            return;
        }
        let params: Vec<u32> = self
            .csi
            .split(';')
            .map(|part| {
                part.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect();
        // 计数类参数：缺省或 0 均按 1
        let n = |index: usize| params.get(index).copied().unwrap_or(0).max(1) as usize;
        let first = params.first().copied().unwrap_or(0);
        match final_byte {
            'm' => self.sgr(&params),
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
                let blank = Cell {
                    ch: ' ',
                    style: self.blank_style(),
                };
                self.screen[self.row].splice(self.col..self.col, vec![blank; count]);
                self.screen[self.row].truncate(self.cols);
            }
            'P' => {
                let count = n(0).min(self.cols - self.col);
                let blank = Cell {
                    ch: ' ',
                    style: self.blank_style(),
                };
                self.screen[self.row].drain(self.col..self.col + count);
                self.screen[self.row].extend(vec![blank; count]);
            }
            'X' => self.erase_cells(self.row, self.col, self.col + n(0)),
            'S' => self.scroll_up(n(0)),
            'T' => self.scroll_down(n(0)),
            'r' => self.set_margins(first, params.get(1).copied()),
            's' => self.saved = (self.row, self.col, self.style),
            'u' => self.restore_cursor(),
            _ => {}
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

    fn sgr(&mut self, params: &[u32]) {
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.inverse = true,
                21 | 22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                30..=37 => self.style.fg = Some(Color::Indexed((p - 30) as u8)),
                39 => self.style.fg = None,
                40..=47 => self.style.bg = Some(Color::Indexed((p - 40) as u8)),
                49 => self.style.bg = None,
                90..=97 => self.style.fg = Some(Color::Indexed((p - 82) as u8)),
                100..=107 => self.style.bg = Some(Color::Indexed((p - 92) as u8)),
                38 | 48 => {
                    let color = match params.get(i + 1) {
                        Some(5) => params.get(i + 2).map(|&v| Color::Indexed(v as u8)),
                        Some(2) => {
                            match (params.get(i + 2), params.get(i + 3), params.get(i + 4)) {
                                (Some(&r), Some(&g), Some(&b)) => {
                                    Some(Color::Rgb(Rgb(r as u8, g as u8, b as u8)))
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(color) = color {
                        if p == 38 {
                            self.style.fg = Some(color);
                        } else {
                            self.style.bg = Some(color);
                        }
                    }
                    i += match params.get(i + 1) {
                        Some(2) => 4,
                        Some(5) => 2,
                        _ => 0,
                    };
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// 把一行单元格合并为若干 [`Span`]，并裁掉行尾无底色的空白。
fn collapse(cells: &[Cell]) -> Line {
    let last = cells
        .iter()
        .rposition(|c| c.ch != ' ' || c.style.bg.is_some() || c.style.inverse);
    let Some(last) = last else {
        return Vec::new();
    };
    let mut line: Line = Vec::new();
    let mut text = String::new();
    let mut current: Option<Style> = None;
    for cell in &cells[..=last] {
        match current {
            Some(style) if style == cell.style => text.push(cell.ch),
            _ => {
                if let Some(style) = current.take() {
                    line.push(Span {
                        text: std::mem::take(&mut text),
                        style,
                    });
                }
                current = Some(cell.style);
                text.push(cell.ch);
            }
        }
    }
    if let Some(style) = current {
        line.push(Span { text, style });
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    fn screen(text: &str) -> VtScreen {
        let mut vt = VtScreen::new(20, 4);
        vt.feed(text);
        vt
    }

    fn render(vt: &VtScreen) -> Vec<String> {
        vt.render_lines(usize::MAX).iter().map(plain).collect()
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
        let line = &vt.render_lines(usize::MAX)[0];
        assert_eq!(line.len(), 2);
        assert_eq!(line[0].text, "red");
        assert_eq!(line[0].style.fg, Some(Color::Indexed(1)));
        assert_eq!(
            line[0].style.resolve_fg_only(DEFAULT_FG, DEFAULT_BG),
            palette(1)
        );
        assert_eq!(line[1].text, " plain");
        assert_eq!(line[1].style, Style::default());
    }

    #[test]
    fn extended_colors_bold_inverse_dim() {
        let vt = screen("\x1b[1;31mA\x1b[38;5;196mB\x1b[48;2;1;2;3mC\x1b[7mD");
        let line = &vt.render_lines(usize::MAX)[0];
        assert_eq!(plain(line), "ABCD");
        // 粗体 + 标准红 → 亮红
        assert_eq!(
            line[0].style.resolve_fg_only(DEFAULT_FG, DEFAULT_BG),
            palette(9)
        );
        assert_eq!(line[1].style.fg, Some(Color::Indexed(196)));
        assert_eq!(
            line[2].style.bg,
            Some(Color::Rgb(Rgb(1, 2, 3))),
            "真彩色背景"
        );
        // 仅前景渲染时反显保持前景色可见；未设前景则提亮为白色
        assert_eq!(
            line[3].style.resolve_fg_only(DEFAULT_FG, DEFAULT_BG),
            palette(196)
        );
        let plain_inverse = screen("\x1b[7mx")
            .render_lines(usize::MAX)
            .remove(0)
            .remove(0)
            .style;
        assert_eq!(
            plain_inverse.resolve_fg_only(DEFAULT_FG, DEFAULT_BG),
            Rgb(255, 255, 255)
        );
        // 暗淡：前景向背景折半
        let dim = screen("\x1b[2;37mx")
            .render_lines(usize::MAX)
            .remove(0)
            .remove(0)
            .style;
        assert_eq!(
            dim.resolve_fg_only(DEFAULT_FG, DEFAULT_BG),
            mix(palette(7), DEFAULT_BG)
        );
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
            [collapse(&cells_of("1")), collapse(&cells_of("2"))]
        );
        assert_eq!(render(&vt), ["1", "2", "3", "4", "5", "6"]);
        // 历史有上限
        let mut long = VtScreen::new(8, 2);
        for i in 0..(SCROLLBACK_CAP + 10) {
            long.feed(&format!("{i}\r\n"));
        }
        assert_eq!(long.scrollback.len(), SCROLLBACK_CAP);
    }

    fn cells_of(text: &str) -> Vec<Cell> {
        let mut cells = vec![Cell::default(); 20];
        for (cell, ch) in cells.iter_mut().zip(text.chars()) {
            cell.ch = ch;
        }
        cells
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
        assert_eq!(
            render(&screen("\x1b[?25l\x1b[?1049h\x1b(B\x1b[5nvisible"))[0],
            "visible"
        );
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
        assert_eq!(line[1].style.fg, Some(Color::Indexed(1)));
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
}

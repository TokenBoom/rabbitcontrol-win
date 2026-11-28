//! 调试用：终端面板的演示脚本与本地回显（`rabbitcontrol-win --demo-terminal`，仅调试构建）。
//!
//! Windows 被控端没有终端服务，本机没法做端到端的终端联调；这里把一段有代表性的远端输出
//! （颜色 / 全角字符 / 制表符对齐 / 全屏程序 / 滚动历史）按节奏喂给屏幕模型，再把键入的字符
//! 像 shell 那样回显出来，用来在真机上核对渲染与输入。

use std::collections::VecDeque;

use super::vt::char_width;

/// 演示用的提示符（绿色用户名 + 蓝色目录，与常见的 zsh / bash 配色一致）。
const PROMPT: &str = "\x1b[1;32mluoxiao@macbook\x1b[0m:\x1b[1;34m~\x1b[0m$ ";

/// 演示脚本：远端「一段一段送来」的输出，按 tick 逐段喂给屏幕模型。
fn script() -> VecDeque<Vec<u8>> {
    let mut parts: Vec<String> = Vec::new();

    // 开括号粘贴：之后粘贴的文本会被包起来（见 DemoShell::input）
    parts.push("\x1b[?2004h".to_string());
    parts.push("\x1b]0;演示终端 — rabbitcontrol\x07".to_string());
    parts.push("Last login: Fri Nov 28 09:12:04 on ttys002\r\n".to_string());
    parts.push(format!("{PROMPT}ls -G\r\n"));
    parts.push(
        "\x1b[1;34mApplications\x1b[0m    \x1b[1;34mDesktop\x1b[0m       \x1b[1;34mDocuments\x1b[0m      \
         \x1b[1;34mDownloads\x1b[0m\r\n"
            .to_string(),
    );
    parts.push(
        "\x1b[1;34mLibrary\x1b[0m         \x1b[1;34mMovies\x1b[0m         \x1b[1;34mMusic\x1b[0m     \
         \x1b[1;34mPictures\x1b[0m        \x1b[1;34mPublic\x1b[0m\r\n"
            .to_string(),
    );
    parts.push(
        "\x1b[1;32minstall-rabbitcontrol.sh\x1b[0m  \x1b[1;31mbackup-2026-11-27.tar.gz\x1b[0m  \
         \x1b[35mnotes.md\x1b[0m  \x1b[1;33m截图 2026-11-28 09.04.12.png\x1b[0m\r\n"
            .to_string(),
    );
    parts.push(format!("{PROMPT}grep -n 'fn main' src/*.rs | head -3\r\n"));
    parts.push(
        "src/main.rs:12:\x1b[01;31mfn main\x1b[0m() {\r\n\
         src/lib.rs:4:\x1b[01;31mfn main\x1b[0m_loop() {\r\n\
         src/ui/shell.rs:88:pub \x1b[01;31mfn main\x1b[0m_wndproc(hwnd: HWND) {\r\n"
            .to_string(),
    );
    parts.push(format!(
        "{PROMPT}echo \"中文与 English 混排，全角标点：「你好，世界」…\"\r\n"
    ));
    parts.push("中文与 English 混排，全角标点：「你好，世界」…\r\n".to_string());
    parts.push(format!("{PROMPT}python3 report.py\r\n"));
    parts.push(
        "名称\t数量\t单价\t小计\r\n\
         机械键盘\t2\t¥399\t¥798\r\n\
         显示器\t1\t¥2999\t¥2999\r\n\
         无线鼠标\t4\t¥99\t¥396\r\n\
         合计\t\t\t¥4193\r\n"
            .to_string(),
    );
    parts.push(format!("{PROMPT}./colors --all\r\n"));
    // 16 色：前景 / 底色 / 亮色各一行
    parts.push("前景 ".to_string());
    for code in 30..38 {
        parts.push(format!("\x1b[{code}m ██ {code} "));
    }
    parts.push("\x1b[0m\r\n底色 ".to_string());
    for code in 40..48 {
        parts.push(format!("\x1b[{code}m   {code} "));
    }
    parts.push("\x1b[0m\r\n亮色 ".to_string());
    for code in 90..98 {
        parts.push(format!("\x1b[{code}m ██ {code} "));
    }
    parts.push("\x1b[0m\r\n".to_string());
    // 256 色与真彩色
    let mut ramp = String::new();
    for color in 16..52 {
        ramp.push_str(&format!("\x1b[48;5;{color}m "));
    }
    parts.push(format!("{ramp}\x1b[0m\r\n"));
    let mut rainbow = String::new();
    for step in 0..32u32 {
        let (r, g, b) = (255u8, (step * 8) as u8, (255 - step * 8) as u8);
        rainbow.push_str(&format!("\x1b[48;2;{r};{g};{b}m "));
    }
    parts.push(format!("{rainbow}\x1b[0m\r\n"));
    parts.push(
        "属性 \x1b[1m粗体\x1b[0m \x1b[2m暗淡\x1b[0m \x1b[3m斜体\x1b[0m \x1b[4m下划线\x1b[0m \
         \x1b[9m删除线\x1b[0m \x1b[7m反显\x1b[0m \x1b[1;4;31m粗体下划线红\x1b[0m\r\n"
            .to_string(),
    );
    parts.push(format!("{PROMPT}htop --demo\r\n"));
    // 全屏程序：备用屏幕 + 制表符框线
    let frame = [
        "╭────────────────────────────── CPU ──────────────────────────────╮",
        "│  [|||||||||||||||||||||||||||          61%]   任务 128   负载 2.14 │",
        "│  [||||||||                    23%]   线程 612   运行 3:12:05      │",
        "├────────────────────────────── 内存 ──────────────────────────────┤",
        "│  [||||||||||||||||||||||||||||||||||||||||||  11.4G / 16G]        │",
        "╰──────────────────────────────────────────────────────────────────╯",
    ]
    .join("\r\n");
    parts.push(format!(
        "\x1b[?1049h\x1b[H\x1b[36m{frame}\x1b[0m\r\n\r\n\x1b[90mF1 帮助  F2 设置  F3 搜索  F9 杀死  F10 退出\x1b[0m\r\n"
    ));
    // 进度条：保存 / 恢复光标原地刷新
    parts.push("部署中 ".to_string());
    for step in 0..=10 {
        let done = "█".repeat(step);
        let left = "░".repeat(10 - step);
        parts.push(format!(
            "\x1b[s\x1b[1;32m[{done}{left}]\x1b[0m {step:>3}0%\x1b[u"
        ));
    }
    parts.push("\x1b[?1049l".to_string());
    parts.push(format!("\r\n{PROMPT}git log --oneline | head -3\r\n"));
    parts.push(
        "\x1b[33ma4c7d9a\x1b[0m \x1b[32mfeat(ui):\x1b[0m 安全页接通剩余设置项\r\n\
         \x1b[33m6de4c23\x1b[0m \x1b[32mfeat(host):\x1b[0m 跨进程 P2P UDP 端口租约\r\n\
         \x1b[33mfe82401\x1b[0m \x1b[32mchore(deps):\x1b[0m 升级 core 到 v0.2.7\r\n"
            .to_string(),
    );
    parts.push(format!(
        "{PROMPT}for i in $(seq 1 80); do echo \"第 $i 行：滚动历史，滚轮翻看 ↑\"; done\r\n"
    ));
    // 滚动历史
    for n in 1..=80 {
        parts.push(format!("第 {n} 行：滚动历史，滚轮翻看 ↑\r\n"));
    }
    parts.push(PROMPT.to_string());
    parts.into_iter().map(String::into_bytes).collect()
}

/// 演示用的「shell」：记住当前输入行，把键入的字符像真终端那样回显出来。
#[derive(Debug, Default)]
pub struct DemoShell {
    line: String,
}

impl DemoShell {
    /// 把一段输入翻译成远端会给回的输出。
    pub fn input(&mut self, bytes: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut out = String::new();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            if let Some(pasted) = rest.strip_prefix("\x1b[200~") {
                // 括号粘贴：剥掉包装按普通字符回显
                let end = pasted.find("\x1b[201~").unwrap_or(pasted.len());
                self.echo(&pasted[..end], &mut out);
                rest = pasted[end..]
                    .strip_prefix("\x1b[201~")
                    .unwrap_or(&pasted[end..]);
            } else if rest.starts_with('\x1b') {
                // 其它转义序列（方向键、功能键……）：原样交给屏幕模型
                let len = escape_len(rest);
                out.push_str(&rest[..len]);
                rest = &rest[len..];
            } else {
                let len = rest
                    .char_indices()
                    .find(|(_, ch)| *ch == '\x1b')
                    .map_or(rest.len(), |(at, _)| at);
                self.echo(&rest[..len], &mut out);
                rest = &rest[len..];
            }
        }
        out.into_bytes()
    }

    fn echo(&mut self, text: &str, out: &mut String) {
        for ch in text.chars() {
            match ch {
                '\r' | '\n' => {
                    out.push_str("\r\n");
                    out.push_str(PROMPT);
                    self.line.clear();
                }
                '\x7f' | '\x08' => {
                    if let Some(removed) = self.line.pop() {
                        // 全角字符占两格：退两格、擦两格、再退两格
                        let cells = char_width(removed).max(1);
                        out.push_str(&"\u{8}".repeat(cells));
                        out.push_str(&" ".repeat(cells));
                        out.push_str(&"\u{8}".repeat(cells));
                    }
                }
                '\x03' => {
                    out.push_str("^C\r\n");
                    out.push_str(PROMPT);
                    self.line.clear();
                }
                '\x04' => out.push_str("logout\r\n"),
                '\x0c' => {
                    out.push_str("\x1b[2J\x1b[H");
                    out.push_str(PROMPT);
                }
                // 其它控制字符（Tab 除外）：终端不回显
                ch if ch.is_control() && ch != '\t' => {}
                ch => {
                    self.line.push(ch);
                    out.push(ch);
                }
            }
        }
    }
}

/// `text` 开头的转义序列有多长（字节）。
fn escape_len(text: &str) -> usize {
    let mut chars = text.char_indices().skip(1);
    match chars.next() {
        // CSI：到第一个 0x40–0x7E 的字节为止
        Some((_, '[')) => text
            .char_indices()
            .skip(2)
            .find(|(_, ch)| ('@'..='~').contains(ch))
            .map_or(text.len(), |(at, ch)| at + ch.len_utf8()),
        // ESC O X
        Some((_, 'O')) => chars
            .next()
            .map_or(text.len(), |(at, ch)| at + ch.len_utf8()),
        Some((at, ch)) => at + ch.len_utf8(),
        None => text.len(),
    }
}

/// 演示的运行状态：还没喂完的脚本 + 回显用的 shell。
#[derive(Debug, Default)]
pub struct Demo {
    shell: DemoShell,
    pending: VecDeque<Vec<u8>>,
}

impl Demo {
    /// 从头开始。
    pub fn new() -> Self {
        Self {
            shell: DemoShell::default(),
            pending: script(),
        }
    }

    /// 取下一段远端输出；脚本放完了为 `None`。
    pub fn next_chunk(&mut self) -> Option<Vec<u8>> {
        self.pending.pop_front()
    }

    /// 回显一段键入。
    pub fn echo(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.shell.input(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::vt::VtScreen;
    use super::*;

    /// 拼接「回显 + 提示符」。
    fn echoed(text: &str) -> Vec<u8> {
        format!("{text}{PROMPT}").into_bytes()
    }

    /// 整段脚本能被屏幕模型吃下：全屏程序退出后主屏内容还在，括号粘贴模式开着。
    #[test]
    fn script_renders_through_the_screen_model() {
        let mut vt = VtScreen::new(120, 30);
        let mut saw_alternate = false;
        for chunk in script() {
            vt.feed_bytes(&chunk);
            saw_alternate |= vt.is_alternate();
        }
        assert!(saw_alternate, "脚本会进一次备用屏幕");
        assert!(!vt.is_alternate(), "结束时已经退回主屏");
        assert!(vt.bracketed_paste());
        assert!(vt.cursor_visible());
        let text: Vec<String> = (0..vt.line_count()).map(|row| vt.line_text(row)).collect();
        assert!(text[0].starts_with("Last login"), "{}", text[0]);
        assert!(text.iter().any(|line| line.contains("中文与 English 混排")));
        assert!(text.iter().any(|line| line.contains("机械键盘")));
        assert!(text.iter().any(|line| line.contains("第 80 行")));
    }

    #[test]
    fn typed_text_is_echoed_like_a_shell() {
        let mut shell = DemoShell::default();
        assert_eq!(shell.input(b"ls -la"), b"ls -la".to_vec());
        assert_eq!(shell.input(b"\r"), echoed("\r\n"));
        // 退格：删掉最后一个字符
        let _ = shell.input(b"ls -la");
        assert_eq!(shell.input(b"\x7f"), b"\x08 \x08".to_vec());
        assert_eq!(
            shell.input(b"\x7f\x7f\x7f\x7f\x7f"),
            "\x08 \x08".repeat(5).into_bytes()
        );
        assert_eq!(shell.input(b"\x7f"), b"", "行已经空了：不再退");
        // 全角字符退格占两格
        let _ = shell.input("中文".as_bytes());
        assert_eq!(shell.input(b"\x7f"), b"\x08\x08  \x08\x08".to_vec());
        // Ctrl+C / Ctrl+D / Ctrl+L
        assert_eq!(shell.input(b"\x03"), echoed("^C\r\n"));
        assert_eq!(shell.input(b"\x04"), b"logout\r\n".to_vec());
        assert_eq!(shell.input(b"\x0c"), echoed("\x1b[2J\x1b[H"));
        // 方向键原样交给屏幕模型；回车后另起一行
        assert_eq!(shell.input(b"\x1b[Ax"), b"\x1b[Ax".to_vec());
        assert_eq!(shell.input(b"\r"), echoed("\r\n"));
        // 括号粘贴：包装被剥掉，换行按回车处理
        assert_eq!(
            shell.input(b"\x1b[200~echo hi\n\x1b[201~"),
            echoed("echo hi\r\n")
        );
    }

    #[test]
    fn escape_lengths_are_measured_in_bytes() {
        assert_eq!(escape_len("\x1b[A"), 3);
        assert_eq!(escape_len("\x1b[1;5A"), 6);
        assert_eq!(escape_len("\x1bOA"), 3);
        assert_eq!(escape_len("\x1b7"), 2);
        assert_eq!(escape_len("\x1b"), 1);
    }
}

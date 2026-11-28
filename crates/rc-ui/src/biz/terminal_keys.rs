//! 终端按键编码：把一次按键（虚拟键码 + 修饰键）翻译成远端终端期望的字节。
//!
//! 纯逻辑、无 GUI 依赖，可单测。口径与 xterm.js（macOS 端终端引擎）在 Windows / Linux 上的行为一致：
//!
//! - **可打印字符不走这里**：没有修饰键或只有 Shift 的字符键交给输入框（[`KeyAction::Compose`]），
//!   由键盘布局与输入法解析成字符后沿文本路径发送——中文输入法上屏、非美式布局都因此可用；
//! - **Ctrl 组合按虚拟键码出控制码**，与键盘布局无关：`Ctrl+A..Z` → `0x01..0x1A`，
//!   `Ctrl+Space` → `0x00`，`Ctrl+[ \ ]`（即 `Ctrl+3 4 5`）→ `0x1B..0x1D`，`Ctrl+6 7` → `0x1E 0x1F`，
//!   `Ctrl+8` → `0x7F`，`Ctrl+Shift+-`（`_`）→ `0x1F`，`Ctrl+Shift+2`（`@`）→ `0x00`；
//!   其余 Ctrl+Shift 组合没有约定编码，吞掉；
//! - **Alt+字符** = `ESC` 前缀 + 美式键位表的字符（xterm.js 的做法）；AltGr（Ctrl+Alt）是布局字符，
//!   同样交给输入框；
//! - `Enter` → CR、`Tab` → TAB、`Shift+Tab` → `ESC [ Z`、`Backspace` → DEL（Ctrl 为 BS）、`Esc` → ESC；
//! - 方向键 → `ESC [ A..D`（应用光标键模式下 `ESC O A..D`），`Home` / `End` → `ESC [ H` / `ESC [ F`
//!   （应用模式 `ESC O H` / `ESC O F`）；带修饰键时统一是 `ESC [ 1 ; m X`；
//! - `Insert` `Delete` `PageUp` `PageDown` `F5..F12` → `ESC [ n ~`，带修饰键时是 `ESC [ n ; m ~`
//!   （数字在前）；`F1..F4` → `ESC O P..S`，带修饰键时 `ESC [ 1 ; m P..S`；
//!   修饰键编号 m = 1 + Shift + 2×Alt + 4×Ctrl；
//! - 单独的 Alt+方向键改发 Ctrl+方向键（向左 / 向右移动一词——xterm.js 在非 mac 平台的惯例）。
//!
//! 本地快捷键（不发给远端）：`Ctrl+Shift+C` / `Ctrl+Insert` 复制选区，`Ctrl+Shift+V` / `Ctrl+V` /
//! `Shift+Insert` 粘贴，`Shift+PageUp` / `Shift+PageDown` 滚动历史。
//!
//! 输入法自己的控制键（韩文 / 汉字 / 转换等）与未知按键都放行给输入框，不在这里吞。

/// 一次按键的翻译结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// 发给远端的字节。
    Send(Vec<u8>),
    /// 交给输入框解析成字符（可打印键：键盘布局、输入法在这里起作用），随后沿文本路径发送。
    Compose,
    /// 本地复制选区。
    Copy,
    /// 本地粘贴剪贴板。
    Paste,
    /// 本地滚动历史：-1 向上翻一屏，+1 向下翻一屏。
    ScrollPage(i8),
    /// 吞掉（修饰键、系统键、没有约定编码的组合键），也不进文本路径。
    Ignore,
}

impl KeyAction {
    /// 按键是否已被本地处理完（为真时不该再进输入框的文本路径）。
    pub fn is_consumed(&self) -> bool {
        !matches!(self, KeyAction::Compose)
    }
}

/// 修饰键状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    /// Shift。
    pub shift: bool,
    /// Ctrl。
    pub ctrl: bool,
    /// Alt。
    pub alt: bool,
}

// 虚拟键码（WinUser.h）
const VK_BACK: u32 = 0x08;
const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_PAUSE: u32 = 0x13;
const VK_CAPITAL: u32 = 0x14;
const VK_PRIOR: u32 = 0x21;
const VK_NEXT: u32 = 0x22;
const VK_END: u32 = 0x23;
const VK_HOME: u32 = 0x24;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_SNAPSHOT: u32 = 0x2C;
const VK_INSERT: u32 = 0x2D;
const VK_DELETE: u32 = 0x2E;
const VK_0: u32 = 0x30;
const VK_2: u32 = 0x32;
const VK_3: u32 = 0x33;
const VK_8: u32 = 0x38;
const VK_C: u32 = 0x43;
const VK_V: u32 = 0x56;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_APPS: u32 = 0x5D;
const VK_SLEEP: u32 = 0x5F;
const VK_NUMLOCK: u32 = 0x90;
const VK_SCROLL: u32 = 0x91;
const VK_OEM_MINUS: u32 = 0xBD;
const VK_OEM_4: u32 = 0xDB;
const VK_OEM_5: u32 = 0xDC;
const VK_OEM_6: u32 = 0xDD;
const VK_F1: u32 = 0x70;
const VK_F4: u32 = 0x73;
const VK_F5: u32 = 0x74;
const VK_F12: u32 = 0x7B;
/// `F5`–`F12` 在 `ESC [ n ~` 里的编号（不连续：没有 16、22）。
const F5_NUMBERS: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];

/// xterm 的修饰键编号：1 + Shift×1 + Alt×2 + Ctrl×4（无修饰键时为 1，序列里省略）。
fn modifier_code(mods: Modifiers) -> u8 {
    1 + u8::from(mods.shift) + 2 * u8::from(mods.alt) + 4 * u8::from(mods.ctrl)
}

fn send(text: String) -> KeyAction {
    KeyAction::Send(text.into_bytes())
}

/// `ESC [ n ~`（Delete / PageUp / PageDown / F5–F12），带修饰键时 `ESC [ n ; m ~`。
fn tilde_key(number: u8, mods: Modifiers) -> KeyAction {
    match modifier_code(mods) {
        1 => send(format!("\x1b[{number}~")),
        m => send(format!("\x1b[{number};{m}~")),
    }
}

/// 不带修饰键时发 `plain`（如 `ESC [ A`、`ESC O H`），带修饰键时发 `ESC [ 1 ; m <final>`
/// （方向键 / Home / End / F1–F4）。
fn cursor_key(plain: String, final_byte: char, mods: Modifiers) -> KeyAction {
    match modifier_code(mods) {
        1 => send(plain),
        m => send(format!("\x1b[1;{m}{final_byte}")),
    }
}

/// 翻译一次按键。
///
/// - `code`：虚拟键码；
/// - `mods`：此刻按着的修饰键；
/// - `app_cursor_keys`：远端是否要求应用光标键模式（`ESC O A` 而不是 `ESC [ A`）。
pub fn encode_key(code: u32, mods: Modifiers, app_cursor_keys: bool) -> KeyAction {
    // AltGr（Ctrl+Alt）打出的是当前键盘布局的字符：交给输入框
    if mods.ctrl && mods.alt {
        return KeyAction::Compose;
    }
    // 本地快捷键
    if code == VK_INSERT && mods.shift != mods.ctrl {
        return if mods.shift {
            KeyAction::Paste
        } else {
            KeyAction::Copy
        };
    }
    if mods.ctrl && (code == VK_V || (code == VK_C && mods.shift)) {
        return if code == VK_V {
            KeyAction::Paste
        } else {
            KeyAction::Copy
        };
    }
    let plain = Modifiers::default();
    let ctrl = Modifiers {
        ctrl: true,
        ..plain
    };
    let alt_only = mods.alt && !mods.shift && !mods.ctrl;
    match code {
        VK_BACK => send(format!(
            "{}{}",
            if mods.alt { "\x1b" } else { "" },
            if mods.ctrl { "\x08" } else { "\x7f" }
        )),
        VK_TAB if mods.shift && !mods.ctrl && !mods.alt => send("\x1b[Z".to_string()),
        VK_TAB => send("\t".to_string()),
        VK_RETURN => send(if mods.alt { "\x1b\r" } else { "\r" }.to_string()),
        VK_ESCAPE => send(if mods.alt { "\x1b\x1b" } else { "\x1b" }.to_string()),
        VK_LEFT | VK_UP | VK_RIGHT | VK_DOWN => {
            let letter = match code {
                VK_UP => 'A',
                VK_DOWN => 'B',
                VK_RIGHT => 'C',
                _ => 'D',
            };
            // 非 mac 平台上 xterm.js 把单独的 Alt+方向键改发 Ctrl+方向键（向左 / 向右移动一词）
            let mods = if alt_only { ctrl } else { mods };
            if app_cursor_keys && mods == plain {
                send(format!("\x1bO{letter}"))
            } else {
                cursor_key(format!("\x1b[{letter}"), letter, mods)
            }
        }
        VK_INSERT => send("\x1b[2~".to_string()),
        VK_DELETE => tilde_key(3, mods),
        VK_HOME => {
            if mods == plain && app_cursor_keys {
                send("\x1bOH".to_string())
            } else {
                cursor_key("\x1b[H".to_string(), 'H', mods)
            }
        }
        VK_END => {
            if mods == plain && app_cursor_keys {
                send("\x1bOF".to_string())
            } else {
                cursor_key("\x1b[F".to_string(), 'F', mods)
            }
        }
        // Shift+PageUp / PageDown 是本地滚动；与 xterm.js 一致，只有 Ctrl 会改变发送的序列
        VK_PRIOR if mods.shift => KeyAction::ScrollPage(-1),
        VK_NEXT if mods.shift => KeyAction::ScrollPage(1),
        VK_PRIOR => tilde_key(5, Modifiers { alt: false, ..mods }),
        VK_NEXT => tilde_key(6, Modifiers { alt: false, ..mods }),
        VK_F1..=VK_F4 => {
            let final_byte = char::from(b'P' + (code - VK_F1) as u8);
            cursor_key(format!("\x1bO{final_byte}"), final_byte, mods)
        }
        VK_F5..=VK_F12 => tilde_key(F5_NUMBERS[(code - VK_F5) as usize], mods),
        // 纯修饰键与系统键：吞掉。其余未知按键放行给输入框——输入法自己的控制键不能拦
        0x10 | 0x11 | 0x12 | VK_PAUSE | VK_CAPITAL | VK_SNAPSHOT | VK_LWIN | VK_RWIN | VK_APPS
        | VK_SLEEP | VK_NUMLOCK | VK_SCROLL => KeyAction::Ignore,
        _ => character_key(code, mods),
    }
}

/// 字符键（字母 / 数字 / 符号 / 空格 / 小键盘）。
fn character_key(code: u32, mods: Modifiers) -> KeyAction {
    if mods.ctrl && !mods.alt {
        // 纯 Ctrl 组合：按虚拟键码出控制码，与键盘布局无关（加了 Shift 就没有约定编码了）
        let control = |value: u8| KeyAction::Send(vec![value]);
        match code {
            0x41..=0x5A if !mods.shift => control((code - 0x40) as u8),
            0x20 if !mods.shift => control(0),
            VK_3..=0x37 if !mods.shift => control((code - VK_3) as u8 + 0x1B),
            VK_8 if !mods.shift => control(0x7F),
            VK_OEM_4 if !mods.shift => control(0x1B),
            VK_OEM_5 if !mods.shift => control(0x1C),
            VK_OEM_6 if !mods.shift => control(0x1D),
            VK_OEM_MINUS if mods.shift => control(0x1F),
            VK_2 if mods.shift => control(0),
            _ => KeyAction::Ignore,
        }
    } else if mods.alt {
        match keycap(code, mods.shift) {
            Some(ch) => send(format!("\x1b{ch}")),
            None => KeyAction::Ignore,
        }
    } else {
        KeyAction::Compose
    }
}

/// 美式布局键位（xterm.js 的键位表）：字母按虚拟键码，数字与符号查表。
fn keycap(code: u32, shift: bool) -> Option<char> {
    const DIGITS: [(char, char); 10] = [
        ('0', ')'),
        ('1', '!'),
        ('2', '@'),
        ('3', '#'),
        ('4', '$'),
        ('5', '%'),
        ('6', '^'),
        ('7', '&'),
        ('8', '*'),
        ('9', '('),
    ];
    const SYMBOLS: [(u32, char, char); 11] = [
        (0xBA, ';', ':'),
        (0xBB, '=', '+'),
        (0xBC, ',', '<'),
        (VK_OEM_MINUS, '-', '_'),
        (0xBE, '.', '>'),
        (0xBF, '/', '?'),
        (0xC0, '`', '~'),
        (VK_OEM_4, '[', '{'),
        (VK_OEM_5, '\\', '|'),
        (VK_OEM_6, ']', '}'),
        (0xDE, '\'', '"'),
    ];
    match code {
        0x20 => Some(' '),
        VK_0..=0x39 => {
            let (plain, shifted) = DIGITS[(code - VK_0) as usize];
            Some(if shift { shifted } else { plain })
        }
        0x41..=0x5A => {
            let letter = char::from(b'a' + (code - 0x41) as u8);
            Some(if shift {
                letter.to_ascii_uppercase()
            } else {
                letter
            })
        }
        _ => SYMBOLS
            .iter()
            .find(|(vk, _, _)| *vk == code)
            .map(|(_, plain, shifted)| if shift { shifted } else { plain })
            .copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
    };
    const SHIFT: Modifiers = Modifiers {
        shift: true,
        ..NONE
    };
    const CTRL: Modifiers = Modifiers { ctrl: true, ..NONE };
    const ALT: Modifiers = Modifiers { alt: true, ..NONE };
    const CTRL_SHIFT: Modifiers = Modifiers {
        ctrl: true,
        ..SHIFT
    };
    const ALT_SHIFT: Modifiers = Modifiers { alt: true, ..SHIFT };
    const CTRL_ALT: Modifiers = Modifiers { ctrl: true, ..ALT };

    fn sent(code: u32, mods: Modifiers) -> Vec<u8> {
        sent_in(code, mods, false)
    }

    fn sent_in(code: u32, mods: Modifiers, app_cursor_keys: bool) -> Vec<u8> {
        match encode_key(code, mods, app_cursor_keys) {
            KeyAction::Send(bytes) => bytes,
            other => panic!("期望发送，得到 {other:?}"),
        }
    }

    #[test]
    fn printable_keys_go_through_the_text_path() {
        assert_eq!(encode_key(0x41, NONE, false), KeyAction::Compose);
        assert_eq!(encode_key(0x41, SHIFT, false), KeyAction::Compose);
        assert_eq!(
            encode_key(0x31, SHIFT, false),
            KeyAction::Compose,
            "Shift+1 由布局决定是 ! 还是别的"
        );
        assert_eq!(encode_key(0x61, NONE, false), KeyAction::Compose, "小键盘");
        assert!(!KeyAction::Compose.is_consumed());
        assert!(KeyAction::Ignore.is_consumed());
    }

    #[test]
    fn alt_prefixes_the_us_keycap_and_altgr_is_composed() {
        assert_eq!(sent(0x45, ALT), b"\x1be");
        assert_eq!(sent(0x41, ALT_SHIFT), b"\x1bA");
        assert_eq!(sent(0x31, ALT), b"\x1b1");
        assert_eq!(sent(0x20, ALT), b"\x1b ");
        assert_eq!(sent(0xDC, ALT_SHIFT), b"\x1b|");
        // AltGr（Ctrl+Alt）是当前布局的字符（如德语布局的 @ €）：交给输入框
        assert_eq!(encode_key(0x45, CTRL_ALT, false), KeyAction::Compose);
        assert_eq!(
            encode_key(
                0x45,
                Modifiers {
                    ctrl: true,
                    ..ALT_SHIFT
                },
                false
            ),
            KeyAction::Compose
        );
    }

    #[test]
    fn control_combinations_follow_virtual_key_codes() {
        assert_eq!(sent(0x41, CTRL), b"\x01", "Ctrl+A");
        assert_eq!(sent(0x5A, CTRL), b"\x1a", "Ctrl+Z");
        assert_eq!(sent(0x43, CTRL), b"\x03", "Ctrl+C 没有选区时照发（中断）");
        assert_eq!(sent(0x20, CTRL), b"\0", "Ctrl+Space");
        // 与布局无关：按物理键位（虚拟键码）算
        assert_eq!(sent(VK_3, CTRL), b"\x1b", "Ctrl+3 = Ctrl+[");
        assert_eq!(sent(0x34, CTRL), b"\x1c", "Ctrl+4 = Ctrl+\\");
        assert_eq!(sent(0x35, CTRL), b"\x1d", "Ctrl+5 = Ctrl+]");
        assert_eq!(sent(0x36, CTRL), b"\x1e", "Ctrl+6 = Ctrl+^");
        assert_eq!(sent(0x37, CTRL), b"\x1f", "Ctrl+7 = Ctrl+_");
        assert_eq!(sent(VK_8, CTRL), b"\x7f", "Ctrl+8");
        assert_eq!(sent(VK_OEM_4, CTRL), b"\x1b", "Ctrl+[");
        assert_eq!(sent(VK_OEM_5, CTRL), b"\x1c", "Ctrl+\\");
        assert_eq!(sent(VK_OEM_6, CTRL), b"\x1d", "Ctrl+]");
        assert_eq!(
            sent(VK_OEM_MINUS, CTRL_SHIFT),
            b"\x1f",
            "Ctrl+Shift+- = Ctrl+_"
        );
        assert_eq!(sent(VK_2, CTRL_SHIFT), b"\0", "Ctrl+Shift+2 = Ctrl+@");
        // 没有约定编码的组合：吞掉，不进文本路径
        assert_eq!(encode_key(VK_0, CTRL, false), KeyAction::Ignore, "Ctrl+1");
        assert_eq!(encode_key(VK_2, CTRL, false), KeyAction::Ignore, "Ctrl+2");
        assert_eq!(
            encode_key(0x20, CTRL_SHIFT, false),
            KeyAction::Ignore,
            "Ctrl+Shift+Space"
        );
        assert_eq!(
            encode_key(0x37, CTRL_SHIFT, false),
            KeyAction::Ignore,
            "Ctrl+Shift+7"
        );
        assert_eq!(
            encode_key(0x41, CTRL_SHIFT, false),
            KeyAction::Ignore,
            "Ctrl+Shift+A"
        );
        assert_eq!(
            encode_key(VK_OEM_4, CTRL_SHIFT, false),
            KeyAction::Ignore,
            "Ctrl+Shift+["
        );
    }

    #[test]
    fn local_shortcuts_are_not_sent() {
        assert_eq!(
            encode_key(0x43, CTRL_SHIFT, false),
            KeyAction::Copy,
            "Ctrl+Shift+C"
        );
        assert_eq!(
            encode_key(VK_INSERT, CTRL, false),
            KeyAction::Copy,
            "Ctrl+Insert"
        );
        assert_eq!(
            encode_key(0x56, CTRL_SHIFT, false),
            KeyAction::Paste,
            "Ctrl+Shift+V"
        );
        assert_eq!(encode_key(0x56, CTRL, false), KeyAction::Paste, "Ctrl+V");
        assert_eq!(
            encode_key(VK_INSERT, SHIFT, false),
            KeyAction::Paste,
            "Shift+Insert"
        );
        assert_eq!(
            encode_key(0x43, CTRL, false),
            KeyAction::Send(vec![3]),
            "Ctrl+C 不是复制"
        );
    }

    #[test]
    fn editing_keys_and_function_keys_match_xterm() {
        assert_eq!(sent(0x0D, NONE), b"\r");
        assert_eq!(sent(0x0D, CTRL), b"\r", "xterm.js：Ctrl+Enter 也是 CR");
        assert_eq!(sent(0x0D, ALT), b"\x1b\r");
        assert_eq!(sent(0x09, NONE), b"\t");
        assert_eq!(sent(0x09, SHIFT), b"\x1b[Z", "Shift+Tab");
        assert_eq!(sent(0x08, NONE), b"\x7f");
        assert_eq!(sent(0x08, SHIFT), b"\x7f", "Shift+Backspace 仍是 DEL");
        assert_eq!(sent(0x08, CTRL), b"\x08", "Ctrl+Backspace = BS");
        assert_eq!(sent(0x08, ALT), b"\x1b\x7f");
        assert_eq!(sent(0x1B, NONE), b"\x1b");
        assert_eq!(sent(0x1B, ALT), b"\x1b\x1b");
        assert_eq!(sent(0x25, NONE), b"\x1b[D");
        assert_eq!(sent(0x26, NONE), b"\x1b[A");
        assert_eq!(sent(0x27, NONE), b"\x1b[C");
        assert_eq!(sent(0x28, NONE), b"\x1b[B");
        assert_eq!(sent(0x26, CTRL), b"\x1b[1;5A", "Ctrl+↑");
        assert_eq!(sent(0x27, CTRL_SHIFT), b"\x1b[1;6C", "Ctrl+Shift+→");
        assert_eq!(sent(0x25, ALT_SHIFT), b"\x1b[1;4D", "Alt+Shift+←");
        // 单独的 Alt+方向键按 xterm.js 改发 Ctrl+方向键（向左 / 向右移动一词）
        assert_eq!(sent(0x25, ALT), b"\x1b[1;5D");
        assert_eq!(sent(0x26, ALT), b"\x1b[1;5A");
        assert_eq!(sent(0x24, NONE), b"\x1b[H", "Home");
        assert_eq!(sent(0x23, NONE), b"\x1b[F", "End");
        assert_eq!(sent(0x24, CTRL), b"\x1b[1;5H");
        assert_eq!(sent(0x23, SHIFT), b"\x1b[1;2F");
        assert_eq!(sent(0x2D, NONE), b"\x1b[2~");
        assert_eq!(sent(0x2E, NONE), b"\x1b[3~");
        assert_eq!(sent(0x2E, CTRL), b"\x1b[3;5~", "Ctrl+Delete");
        assert_eq!(sent(0x21, NONE), b"\x1b[5~");
        assert_eq!(sent(0x21, CTRL), b"\x1b[5;5~");
        assert_eq!(
            sent(0x21, ALT),
            b"\x1b[5~",
            "与 xterm.js 一致：PageUp 只有 Ctrl 会改变序列"
        );
        assert_eq!(sent(0x22, NONE), b"\x1b[6~");
        assert_eq!(
            encode_key(0x21, SHIFT, false),
            KeyAction::ScrollPage(-1),
            "Shift+PageUp"
        );
        assert_eq!(
            encode_key(0x22, SHIFT, false),
            KeyAction::ScrollPage(1),
            "Shift+PageDown"
        );
        assert_eq!(sent(0x70, NONE), b"\x1bOP", "F1");
        assert_eq!(sent(VK_F4, NONE), b"\x1bOS", "F4");
        assert_eq!(sent(0x70, SHIFT), b"\x1b[1;2P", "Shift+F1");
        assert_eq!(sent(VK_F5, NONE), b"\x1b[15~", "F5");
        assert_eq!(sent(0x78, NONE), b"\x1b[20~", "F9");
        assert_eq!(sent(0x79, NONE), b"\x1b[21~", "F10（没有 22）");
        assert_eq!(sent(VK_F12, NONE), b"\x1b[24~", "F12");
        assert_eq!(sent(VK_F5, SHIFT), b"\x1b[15;2~", "Shift+F5");
        assert_eq!(sent(0x79, CTRL), b"\x1b[21;5~", "Ctrl+F10");
    }

    #[test]
    fn application_cursor_keys_mode_switches_the_encoding() {
        assert_eq!(sent_in(0x26, NONE, true), b"\x1bOA");
        assert_eq!(sent_in(0x25, NONE, true), b"\x1bOD");
        assert_eq!(sent_in(0x24, NONE, true), b"\x1bOH");
        assert_eq!(sent_in(0x23, NONE, true), b"\x1bOF");
        // 带修饰键时不分模式
        assert_eq!(sent_in(0x26, CTRL, true), b"\x1b[1;5A");
        assert_eq!(sent_in(0x24, CTRL, true), b"\x1b[1;5H");
    }

    #[test]
    fn modifier_and_system_keys_are_swallowed() {
        for code in [
            0x10,
            0x11,
            0x12,
            VK_PAUSE,
            VK_CAPITAL,
            VK_SNAPSHOT,
            VK_LWIN,
            VK_RWIN,
            VK_APPS,
            VK_SLEEP,
            VK_NUMLOCK,
            VK_SCROLL,
        ] {
            assert_eq!(
                encode_key(code, NONE, false),
                KeyAction::Ignore,
                "vk {code:#x}"
            );
        }
        // 输入法的控制键等未知按键放行，不能吞
        assert_eq!(
            encode_key(0x15, NONE, false),
            KeyAction::Compose,
            "VK_HANGUL / KANA"
        );
        assert_eq!(
            encode_key(0x1C, NONE, false),
            KeyAction::Compose,
            "VK_CONVERT"
        );
    }
}

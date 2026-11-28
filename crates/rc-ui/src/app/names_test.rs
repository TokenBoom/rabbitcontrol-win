//! 回归测试：界面元素名字的静态核对（不需要 XAML 运行时）。
//!
//! # 为什么需要
//!
//! 登录成功后要往 `AccountEmail` 写账号，而那个元素早在界面改版时就被删掉了。
//! 查找失败返回的错误码是 S_OK（显示成「操作成功完成。」），登录流程静默中断，
//! 任何人都进不了主界面——截图和「只测登录页」的自动化都发现不了。同一个毛病
//! 还让会话窗口起不来（`FileDownloadPick` 在 XAML 里叫另一个名字，接线在那里中断）。
//!
//! 所以这里做两个方向的核对：
//!
//! - 代码里按字面量名字查找的元素，必须存在于 `view.xaml` / `session_window.xaml`；
//! - XAML 里可交互且未禁用的控件，必须被代码按名字引用（否则点了没有反应）。

#![cfg(test)]

use std::path::Path;

const MAIN_XAML: &str = include_str!("../ui/assets/view.xaml");
const SESSION_XAML: &str = include_str!("../ui/assets/session_window.xaml");

/// 第一个参数是元素名字的辅助函数。
const HELPERS: &[&str] = &[
    "label",
    "set_visible",
    "click",
    "set_checked_named",
    "switch_toggled",
];

/// 不带类型标注的 `find("Name"` / `find_opt("Name"`（类型由左侧推断）。
const PLAIN_FIND: &[&str] = &["find", "find_opt"];

/// 需要代码响应的控件标签。
const INTERACTIVE_TAGS: &[&str] = &[
    "Button",
    "ToggleSwitch",
    "ToggleButton",
    "ComboBox",
    "TextBox",
    "PasswordBox",
];

fn collect_rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            // 本文件里的字符串字面量是扫描器的样本，不是真实的界面查找
            && path.file_name().is_some_and(|name| name != "names_test.rs")
        {
            out.push(path);
        }
    }
}

fn xaml_names(text: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("x:Name=\"") {
        let after = &rest[pos + "x:Name=\"".len()..];
        let end = after.find('"').unwrap_or(after.len());
        names.push(&after[..end]);
        rest = &after[end..];
    }
    names
}

fn line_of(text: &str, pos: usize) -> usize {
    text[..pos].bytes().filter(|b| *b == b'\n').count() + 1
}

/// 去掉整行注释，避免把注释里的示例当成真实查找。
fn strip_comment_lines(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `helper("Name"`：辅助函数名后（可跨行）紧跟的字符串字面量。
fn lookups_via_helper(source: &str, helper: &str) -> Vec<(String, usize)> {
    let bytes = source.as_bytes();
    let pattern = format!("{helper}(");
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(pos) = source[from..].find(&pattern) {
        let start = from + pos;
        let mut at = start + pattern.len();
        from = at;
        // 前一个字符不能是标识符字符（排除 `set_label(` 之类）
        if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            continue;
        }
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes.get(at) != Some(&b'"') {
            continue;
        }
        let name_start = at + 1;
        let name_end = name_start + source[name_start..].find('"').unwrap_or(0);
        if name_end > name_start {
            found.push((
                source[name_start..name_end].to_string(),
                line_of(source, start),
            ));
        }
        from = name_end;
    }
    found
}

/// `find::<T>("Name"` / `find_opt::<T>("Name"`：泛型参数之后紧跟的字符串字面量。
fn lookups_via_find(source: &str) -> Vec<(String, usize)> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(pos) = source[from..].find("::<") {
        let start = from + pos;
        // 泛型在调用括号之前（`find::<T>("名字"`）：它前面的文本必须以 `find` / `find_opt` 结尾，
        // 且 `find` 前面不能还是标识符字符
        let head = &source[..start];
        let is_find = ["find", "find_opt"].iter().any(|keyword| {
            head.ends_with(keyword)
                && !head[..head.len() - keyword.len()]
                    .ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        });
        if !is_find {
            from = start + 3;
            continue;
        }
        let Some(close) = source[start..].find('>') else {
            break;
        };
        let mut at = start + close + 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        // 泛型之后是调用括号，再是名字字面量
        if bytes.get(at) != Some(&b'(') {
            from = start + 3;
            continue;
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes.get(at) != Some(&b'"') {
            from = start + 3;
            continue;
        }
        let name_start = at + 1;
        let name_end = name_start + source[name_start..].find('"').unwrap_or(0);
        if name_end > name_start {
            found.push((
                source[name_start..name_end].to_string(),
                line_of(source, start),
            ));
        }
        from = name_end;
    }
    found
}

/// `const NAME: &[&str] = [ "A", "B" ];` 里的全部字符串（按名字循环查找的元素清单）。
fn lookups_via_name_lists(source: &str) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    let mut rest = source;
    let mut offset = 0;
    while let Some(pos) = rest.find("&[&str] = [") {
        let start = offset + pos + "&[&str] = [".len();
        let body = &source[start..];
        let end = body.find("];").unwrap_or(0);
        let mut piece = &body[..end];
        let mut piece_offset = start;
        while let Some(q) = piece.find('"') {
            let after = &piece[q + 1..];
            let close = after.find('"').unwrap_or(0);
            let name = &after[..close];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                found.push((name.to_string(), line_of(source, piece_offset + q)));
            }
            piece = &after[close..];
            piece_offset += q + 1 + close;
        }
        rest = &body[end..];
        offset = start + end;
    }
    found
}

/// XAML 里可交互且未禁用的控件 `(标签, 名字)`。
fn interactive_controls(text: &str) -> Vec<(String, String)> {
    let mut controls = Vec::new();
    for tag in INTERACTIVE_TAGS {
        let mut rest = text;
        let pattern = format!("<{tag}");
        while let Some(pos) = rest.find(&pattern) {
            let after = &rest[pos..];
            let end = after.find('>').unwrap_or(after.len());
            let element = &after[..end];
            // 标签名后必须紧跟空白或属性，排除 `<ButtonX`
            let boundary = element.get(pattern.len()..pattern.len() + 1);
            let is_tag = boundary.is_some_and(|c| c.starts_with(char::is_whitespace));
            if is_tag {
                if let Some(name) = element
                    .find("x:Name=\"")
                    .map(|at| &element[at + 8..])
                    .and_then(|tail| tail.split('"').next())
                {
                    if !element.contains("IsEnabled=\"False\"") {
                        controls.push((tag.to_string(), name.to_string()));
                    }
                }
            }
            rest = &after[end..];
        }
    }
    controls
}

#[test]
fn every_looked_up_element_exists_in_xaml() {
    let defined: std::collections::HashSet<&str> = xaml_names(MAIN_XAML)
        .into_iter()
        .chain(xaml_names(SESSION_XAML))
        .collect();
    assert!(
        defined.len() > 100,
        "XAML 解析异常：只找到 {} 个命名元素",
        defined.len()
    );

    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_rust_files(&crate_dir.join("src").join("app"), &mut files);
    files.push(crate_dir.join("src").join("main.rs"));
    assert!(files.len() > 5, "源码文件数量异常：{}", files.len());

    let mut missing = Vec::new();
    let mut looked_up = 0;
    for file in &files {
        let source = strip_comment_lines(&std::fs::read_to_string(file).expect("读取源码"));
        let relative = file
            .strip_prefix(crate_dir)
            .unwrap_or(file)
            .display()
            .to_string();
        let mut lookups = Vec::new();
        for helper in HELPERS.iter().chain(PLAIN_FIND) {
            lookups.extend(lookups_via_helper(&source, helper));
        }
        lookups.extend(lookups_via_find(&source));
        lookups.extend(lookups_via_name_lists(&source));
        for (name, line) in lookups {
            looked_up += 1;
            if !defined.contains(name.as_str()) {
                missing.push(format!("{name}  ({relative}:{line})"));
            }
        }
    }
    assert!(looked_up > 100, "扫描异常：只找到 {looked_up} 处按名字查找");
    assert!(
        missing.is_empty(),
        "代码里查找的界面元素在 XAML 中不存在（调用会在该处静默失败）：\n{}",
        missing.join("\n")
    );
}

#[test]
fn every_enabled_interactive_control_is_referenced() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = vec![std::fs::read_to_string(crate_dir.join("src").join("main.rs")).unwrap()];
    let mut files = Vec::new();
    collect_rust_files(&crate_dir.join("src").join("app"), &mut files);
    for file in files {
        sources.push(strip_comment_lines(&std::fs::read_to_string(file).unwrap()));
    }
    let sources: Vec<String> = {
        let mut all = sources;
        let mut ui = Vec::new();
        collect_rust_files(&crate_dir.join("src").join("ui"), &mut ui);
        for file in ui {
            if file.file_name().is_some_and(|f| f != "names_test.rs") {
                all.push(strip_comment_lines(&std::fs::read_to_string(file).unwrap()));
            }
        }
        all
    };

    let mut dead = Vec::new();
    for (xaml, label) in [
        (MAIN_XAML, "view.xaml"),
        (SESSION_XAML, "session_window.xaml"),
    ] {
        for (tag, name) in interactive_controls(xaml) {
            let quoted = format!("\"{name}\"");
            if !sources.iter().any(|source| source.contains(&quoted)) {
                dead.push(format!("<{tag} x:Name=\"{name}\" />  ({label})"));
            }
        }
    }
    assert!(
        dead.is_empty(),
        "界面上的可交互控件没有任何代码引用（点了不会有反应）：\n{}",
        dead.join("\n")
    );
}

#[test]
fn the_scanner_itself_works() {
    let source = "// label(\"CommentedOut\", x)\nself.label(\"Multi\", x);\nself.label(\n    \"NextLine\",\n    x,\n)?;\nlet a = self.find::<TextBlock>(\"Found\")?;\nlet b = self.find_opt::<Control>(\n    \"OptFound\",\n)?;\nlet c: TextBlock = self.find(\"PlainFound\")?;\nlet d = self.find_opt(\"PlainOpt\");\nconst BUSY_CONTROLS: &[&str] = [\"A1\", \"A2\"];\nfn set_label() {}\nset_label(\"NotALookup\");\n";
    let stripped = strip_comment_lines(source);
    let mut names: Vec<String> = lookups_via_helper(&stripped, "label")
        .into_iter()
        .chain(
            PLAIN_FIND
                .iter()
                .flat_map(|helper| lookups_via_helper(&stripped, helper)),
        )
        .map(|(name, _)| name)
        .collect();
    names.extend(
        lookups_via_find(&stripped)
            .into_iter()
            .map(|(name, _)| name),
    );
    names.extend(
        lookups_via_name_lists(&stripped)
            .into_iter()
            .map(|(name, _)| name),
    );
    names.sort();
    assert_eq!(
        names,
        [
            "A1",
            "A2",
            "Found",
            "Multi",
            "NextLine",
            "OptFound",
            "PlainFound",
            "PlainOpt",
        ]
    );

    let xaml = "<Button x:Name=\"On\" Content=\"x\"/>\n<Button\n  x:Name=\"Off\"\n  IsEnabled=\"False\" />\n<Button x:Name=\"Split\"\n Content=\"y\" />\n<ToggleSwitch x:Name=\"Switch\" OnContent=\"\"/>\n<Border x:Name=\"NotControl\"/>\n";
    let controls = interactive_controls(xaml);
    assert_eq!(
        controls,
        [
            ("Button".to_string(), "On".to_string()),
            ("Button".to_string(), "Split".to_string()),
            ("ToggleSwitch".to_string(), "Switch".to_string())
        ]
    );
}

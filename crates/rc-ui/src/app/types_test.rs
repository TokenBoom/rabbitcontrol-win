//! 回归测试：代码取界面元素时用的类型，必须与 XAML 里该元素的类型相符（静态核对）。
//!
//! # 为什么需要
//!
//! windows-rs 0.37 投影里类型转换失败只返回 E_NOINTERFACE，运行时在控制台留一行就没了，
//! 而后果可能是整段界面流程中断：给不存在的状态圆点上色让「信令在线」和设备码永远不显示；
//! 把 `ToggleSwitch` 当成 `ToggleButton` 让开关状态永远设不上；把 `TextBlock` 转成
//! `TextElement` 让状态文字颜色设置失败。这些都要跑起来盯着控制台才能发现，
//! 所以在这里按 XAML 标签做一次静态核对。

#![cfg(test)]

use std::path::Path;

const MAIN_XAML: &str = include_str!("../ui/assets/view.xaml");
const SESSION_XAML: &str = include_str!("../ui/assets/session_window.xaml");

/// XAML 标签 → 代码里可以安全转换过去的类型（该类自身 + UWP XAML 继承链上的基类）。
///
/// 没登记的标签返回 `None`（调用方按「无法判断」报告出来，补全这张表即可）。
fn compatible_types(tag: &str) -> Option<&'static [&'static str]> {
    let table: &'static [&'static str] = match tag {
        "TextBlock" => &["TextBlock", "FrameworkElement", "UIElement"],
        "Canvas" => &["Canvas", "Panel", "FrameworkElement", "UIElement"],
        "Image" => &["Image", "FrameworkElement", "UIElement"],
        "Ellipse" => &["Ellipse", "Shape", "FrameworkElement", "UIElement"],
        "Rectangle" => &["Rectangle", "Shape", "FrameworkElement", "UIElement"],
        "Button" => &[
            "Button",
            "ButtonBase",
            "ContentControl",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "ToggleButton" => &[
            "ToggleButton",
            "ButtonBase",
            "ContentControl",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "CheckBox" => &[
            "CheckBox",
            "ToggleButton",
            "ButtonBase",
            "ContentControl",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "RadioButton" => &[
            "RadioButton",
            "ToggleButton",
            "ButtonBase",
            "ContentControl",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "ToggleSwitch" => &[
            "ToggleSwitch",
            "ContentControl",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "TextBox" => &["TextBox", "Control", "FrameworkElement", "UIElement"],
        "PasswordBox" => &["PasswordBox", "Control", "FrameworkElement", "UIElement"],
        "ComboBox" => &[
            "ComboBox",
            "Selector",
            "ItemsControl",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "Border" => &[
            "Border",
            "ContentControl",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "ScrollViewer" => &[
            "ScrollViewer",
            "ContentControl",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "StackPanel" => &["StackPanel", "Panel", "FrameworkElement", "UIElement"],
        "Grid" => &["Grid", "Panel", "FrameworkElement", "UIElement"],
        "SwapChainPanel" => &[
            "SwapChainPanel",
            "Grid",
            "Panel",
            "FrameworkElement",
            "UIElement",
        ],
        "ProgressRing" => &["ProgressRing", "Control", "FrameworkElement", "UIElement"],
        "ProgressBar" => &[
            "ProgressBar",
            "RangeBase",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "FontIcon" => &["FontIcon", "IconElement", "FrameworkElement", "UIElement"],
        "MenuFlyout" => &["MenuFlyout", "FlyoutBase", "DependencyObject"],
        "MenuFlyoutItem" => &[
            "MenuFlyoutItem",
            "MenuFlyoutItemBase",
            "Control",
            "FrameworkElement",
            "UIElement",
        ],
        "Storyboard" => &["Storyboard", "Timeline", "DependencyObject"],
        _ => return None,
    };
    Some(table)
}

/// 全部命名元素 `(名字, XAML 标签)`。
fn xaml_tags(text: &str) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    let mut at = 0;
    while let Some(pos) = text[at..].find('<') {
        let start = at + pos + 1;
        let tag: String = text[start..]
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect();
        let Some(end) = text[start..].find('>') else {
            break;
        };
        at = start + end;
        let element = &text[start..at];
        if tag.is_empty() {
            continue;
        }
        if let Some(name_at) = element.find("x:Name=\"") {
            if let Some(name) = element[name_at + "x:Name=\"".len()..]
                .split('"')
                .next()
                .filter(|name| !name.is_empty())
            {
                tags.push((name.to_string(), tag));
            }
        }
    }
    tags
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == ':' || c == '_'
}

fn quoted_name_at(source: &str, mut at: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    if bytes.get(at) != Some(&b'"') {
        return None;
    }
    let start = at + 1;
    let end = start + source[start..].find('"')?;
    Some((source[start..end].to_string(), end))
}

/// `find::<T>("Name"` / `find_opt::<T>("Name"` → `(名字, 类型, 行)`。
fn typed_find(source: &str) -> Vec<(String, String, usize)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(pos) = source[from..].find("::<") {
        let start = from + pos;
        let head = &source[..start];
        let is_find = head.ends_with("find") || head.ends_with("find_opt");
        let Some(close) = source[start..].find('>') else {
            break;
        };
        from = start + close + 1;
        if !is_find {
            continue;
        }
        let ty = source[start + 3..start + close].to_string();
        // 泛型之后是调用括号，再是名字字面量
        let mut at = start + close + 1;
        while source.as_bytes().get(at) == Some(&b'(') {
            at += 1;
        }
        let Some((name, end)) = quoted_name_at(source, at) else {
            continue;
        };
        found.push((name, ty, super::names_test::line_of(source, start)));
        from = end;
    }
    found
}

/// `let x: Type = self.find("Name")` / `self.find_opt("Name")` / `self.root.FindName("Name")?.cast()`
/// → `(名字, 类型, 行)`（类型由左侧标注给出）。
fn typed_let(source: &str) -> Vec<(String, String, usize)> {
    let mut found = Vec::new();
    for pattern in ["= self.find(", "= self.find_opt(", "= self.root.FindName("] {
        let mut from = 0;
        while let Some(pos) = source[from..].find(pattern) {
            let eq = from + pos;
            let head = &source[..eq];
            from = eq + pattern.len();
            let Some(decl) = head.rfind("let ").map(|at| head[at + 4..].trim_end()) else {
                continue;
            };
            let Some(colon) = decl.find(": ") else {
                continue;
            };
            let ty: String = decl[colon + 2..]
                .trim()
                .chars()
                .take_while(|c| is_ident_char(*c))
                .collect();
            let Some((name, end)) = quoted_name_at(source, eq + pattern.len()) else {
                continue;
            };
            if !ty.is_empty() {
                found.push((name, ty, super::names_test::line_of(source, eq)));
            }
            from = end + 1;
        }
    }
    found
}

#[test]
fn every_lookup_type_matches_the_xaml_element() {
    let tags: std::collections::HashMap<String, String> = xaml_tags(MAIN_XAML)
        .into_iter()
        .chain(xaml_tags(SESSION_XAML))
        .collect();
    assert!(
        tags.len() > 100,
        "XAML 解析异常：只找到 {} 个命名元素",
        tags.len()
    );

    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    super::names_test::collect_rust_files(&crate_dir.join("src").join("app"), &mut files);

    let mut problems = Vec::new();
    let mut checked = 0;
    for file in &files {
        let source = super::names_test::strip_comment_lines(
            &std::fs::read_to_string(file).expect("读取源码"),
        );
        let relative = file
            .strip_prefix(crate_dir)
            .unwrap_or(file)
            .display()
            .to_string();
        let mut lookups = typed_find(&source);
        lookups.extend(typed_let(&source));
        for (name, ty, line) in lookups {
            // 只取类型名的最后一段；单个字母是泛型参数，无法静态核对
            let ty = ty.rsplit("::").next().unwrap_or(&ty);
            if ty.len() <= 1 {
                continue;
            }
            // 名字不存在由 names_test 里的测试负责报告
            let Some(tag) = tags.get(&name) else {
                continue;
            };
            checked += 1;
            match compatible_types(tag) {
                None => problems.push(format!(
                    "{name}: XAML 标签 <{tag}> 还没登记类型表（{relative}:{line}）"
                )),
                Some(allowed) if !allowed.contains(&ty) => problems.push(format!(
                    "{name} 在 XAML 里是 <{tag}>，代码却按 {ty} 取（{relative}:{line}）"
                )),
                Some(_) => {}
            }
        }
    }
    assert!(checked > 40, "扫描异常：只核对了 {checked} 处带类型的查找");
    assert!(
        problems.is_empty(),
        "代码取界面元素时用的类型与 XAML 不符（转换会失败，错误码 E_NOINTERFACE）：\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_type_table_and_the_scanners_themselves_work() {
    let allows = |tag: &str, ty: &str| compatible_types(tag).is_some_and(|ok| ok.contains(&ty));
    // 继承链上可以，链外不行
    assert!(allows("ToggleSwitch", "ToggleSwitch"));
    assert!(allows("ToggleSwitch", "Control"));
    assert!(
        !allows("ToggleSwitch", "ToggleButton"),
        "ToggleSwitch 不是 ToggleButton"
    );
    assert!(
        !allows("TextBlock", "TextElement"),
        "TextBlock 不能转成 TextElement"
    );
    assert!(allows("ComboBox", "Selector"));
    assert!(allows("CheckBox", "ToggleButton"));
    assert!(allows("SwapChainPanel", "Grid"));
    assert!(compatible_types("NotAControl").is_none());

    let tags = xaml_tags(
        "<Grid><ToggleSwitch x:Name=\"Switch\" OnContent=\"\" /><!-- <Fake x:Name=\"No\" /> --><TextBlock\n  x:Name=\"Text\" /></Grid>",
    );
    assert_eq!(
        tags,
        [
            ("Switch".to_string(), "ToggleSwitch".to_string()),
            ("Text".to_string(), "TextBlock".to_string())
        ]
    );

    let source = "let a: ToggleSwitch = self.find(\"Switch\")?;\nlet b = self.find_opt::<windows::UI::Xaml::Controls::TextBlock>(\n    \"Text\",\n)?;\nlet c: Control = self.root.FindName(\"Btn\")?.cast()?;\nlet generic = self.find::<T>(\"Anything\");\n";
    let mut found = typed_find(source);
    found.extend(typed_let(source));
    found.sort();
    assert_eq!(
        found
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Anything", "Btn", "Switch", "Text"]
    );
    assert_eq!(found[1].1, "Control");
    assert_eq!(found[3].1, "windows::UI::Xaml::Controls::TextBlock");
}

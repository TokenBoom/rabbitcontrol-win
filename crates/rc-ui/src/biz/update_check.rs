//! 自动更新检测（M6.3a）：查询 GitHub Releases 最新版。
//!
//! 通过 PowerShell `Invoke-WebRequest` 请求 GitHub API（系统代理直接生效，
//! 不给 rc-ui 增加 HTTP 客户端依赖），再用 core 的版本解析与比较。
//!
//! 当前只做「检测 + 提示」：下载与自替换未实现，「立即更新」打开安装包的下载地址。
//!
//! 仓库没有任何 Release 时 GitHub 对 `releases/latest` 返回 404——这是正常
//! 状态而不是故障，文案上要和网络错误区分开。

#![deny(missing_docs)]

use std::process::Command;

/// 仓库标识（GitHub `owner/repo`）。
const REPO: &str = "TokenBoom/rabbitcontrol-win";
/// 当前版本（Cargo 包版本）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 问题反馈页面（「提交反馈」打开的地址）。
pub const ISSUES_URL: &str = "https://github.com/TokenBoom/rabbitcontrol-win/issues";
/// 开源仓库首页（「关于」里打开的地址）。
pub const REPO_URL: &str = "https://github.com/TokenBoom/rabbitcontrol-win";
/// 单次请求超时（秒）。
const REQUEST_TIMEOUT_SECS: u32 = 20;
/// 更新说明在弹窗里最多显示这么多字符。
const NOTES_LIMIT: usize = 1200;

/// PowerShell 5.1 兼容的请求脚本：首行输出 `STATUS:<HTTP 状态码>`（连不上为 0），
/// 其后是响应正文。
const PS_SCRIPT: &str = r"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$ErrorActionPreference = 'Stop'
try {
  $r = Invoke-WebRequest -Uri '__URL__' -Headers @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'rabbitcontrol-win/__VERSION__' } -UseBasicParsing -TimeoutSec __TIMEOUT__
  'STATUS:' + [int]$r.StatusCode
  $r.Content
} catch {
  $code = 0
  if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
  'STATUS:' + $code
}
";

/// 一次检查的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// 最新发布版本号（不含前导 `v`）；仓库尚无 Release 时为 `None`。
    pub latest: Option<String>,
    /// Releases 页面地址。
    pub url: String,
    /// 是否比当前版本新。
    pub is_newer: bool,
    /// 发布标题（Release 没填标题时是 tag）。
    pub title: String,
    /// 更新说明（Markdown 原文）。
    pub notes: String,
    /// 发布时间（RFC 3339 原文；没取到为空串）。
    pub published_at: String,
    /// 本机对应的安装包下载地址；Release 里没有就为 `None`，改开发布页面。
    pub download_url: Option<String>,
}

impl UpdateCheck {
    /// 设置页「更新」行显示的状态文案。
    pub fn status_text(&self) -> String {
        match &self.latest {
            None => format!("尚无发布版本（当前 {CURRENT_VERSION}）"),
            Some(version) if self.is_newer => format!("新版本 {version} 可用：{}", self.url),
            Some(_) => format!("已是最新版本 {CURRENT_VERSION}"),
        }
    }

    /// 弹窗里的发布日期（`2026-11-28`）：取 RFC 3339 时间的前 10 位，取不到就原样显示。
    pub fn published_date(&self) -> String {
        self.published_at
            .get(..10)
            .unwrap_or(&self.published_at)
            .to_string()
    }

    /// 「立即更新」打开的地址：本机对应的安装包，没有就打开发布页面。
    pub fn install_url(&self) -> &str {
        self.download_url.as_deref().unwrap_or(&self.url)
    }

    /// 弹窗里的更新说明：去掉 Markdown 符号的纯文本（见 [`plain_notes`]）。
    pub fn plain_notes(&self) -> String {
        plain_notes(&self.notes)
    }
}

/// 检查 GitHub Releases 是否有新版本。
///
/// 会发起一次网络请求（≤ 20 秒），调用方须在工作线程执行。
/// 网络错误或被 API 拒绝时返回 `Err(可直接展示的原因)`。
pub fn check() -> Result<UpdateCheck, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let script = PS_SCRIPT
        .replace("__URL__", &url)
        .replace("__VERSION__", CURRENT_VERSION)
        .replace("__TIMEOUT__", &REQUEST_TIMEOUT_SECS.to_string());
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|err| format!("无法启动更新检查：{err}"))?;
    if !output.status.success() {
        return Err("更新检查未能运行".into());
    }
    parse_response(String::from_utf8_lossy(&output.stdout).trim())
}

/// 解析请求脚本的输出（首行 `STATUS:<码>` + 正文）。
fn parse_response(raw: &str) -> Result<UpdateCheck, String> {
    let (status_line, body) = raw.split_once('\n').unwrap_or((raw, ""));
    let status: u16 = status_line
        .trim()
        .strip_prefix("STATUS:")
        .and_then(|code| code.trim().parse().ok())
        .ok_or("更新检查返回了无法识别的内容")?;
    match status {
        200 => {
            let info = rc_client_core::updater::parse_github_release_json(body.trim())
                .map_err(|err| format!("解析 Release 信息失败：{err}"))?;
            Ok(UpdateCheck {
                is_newer: rc_client_core::updater::is_newer_version(&info.version, CURRENT_VERSION),
                url: info.html_url.clone(),
                latest: Some(info.version.clone()),
                title: if info.title.trim().is_empty() {
                    info.tag.clone()
                } else {
                    info.title.clone()
                },
                notes: info.notes.clone(),
                published_at: info.published_at.clone(),
                download_url: pick_installer(&info.assets),
            })
        }
        404 => Ok(UpdateCheck {
            latest: None,
            url: format!("https://github.com/{REPO}/releases"),
            is_newer: false,
            title: String::new(),
            notes: String::new(),
            published_at: String::new(),
            download_url: None,
        }),
        403 | 429 => Err("GitHub API 请求过于频繁，请稍后再试".into()),
        0 => Err("无法连接 GitHub，请检查网络或代理设置".into()),
        other => Err(format!("GitHub API 返回 HTTP {other}")),
    }
}

/// 挑出本机对应的安装包：发布流程打的是 `rabbitcontrol-win-<tag>.zip`。
///
/// 优先名字对得上的 zip；只有别的 zip 也认；一个 zip 都没有就交给发布页面。
fn pick_installer(assets: &[rc_client_core::updater::ReleaseAsset]) -> Option<String> {
    let is_zip = |asset: &&rc_client_core::updater::ReleaseAsset| {
        asset.name.to_ascii_lowercase().ends_with(".zip")
    };
    assets
        .iter()
        .filter(is_zip)
        .find(|asset| {
            asset
                .name
                .to_ascii_lowercase()
                .starts_with("rabbitcontrol-win")
        })
        .or_else(|| assets.iter().find(is_zip))
        .map(|asset| asset.download_url.clone())
}

/// 把 Release 的 Markdown 更新说明整理成适合纯文本显示的样子：
/// 标题加【】、列表项换成圆点、链接只留文字、图片整行去掉、代码块原样保留、空行最多留一个。
pub fn plain_notes(markdown: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_code = false;
    for raw in markdown.lines() {
        let line = raw.trim();
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            lines.push(line.to_string());
            continue;
        }
        if line.is_empty() {
            if lines.last().is_some_and(|last| !last.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }
        let text = strip_links(line);
        let text = text.trim();
        let heading = ["### ", "## ", "# "].iter().find_map(|marker| {
            text.strip_prefix(marker)
                .map(|rest| format!("【{}】", rest.trim()))
        });
        let body = heading.unwrap_or_else(|| text.to_string());
        let body = body
            .strip_prefix("- ")
            .or_else(|| body.strip_prefix("* "))
            .map_or(body.clone(), |item| format!("• {item}"));
        lines.push(body);
    }
    while lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    truncate_chars(&lines.join("\n"), NOTES_LIMIT)
}

/// 超长就按字符截断并补省略号（不会切在半个字符中间）。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit).collect();
    cut.push('…');
    cut
}

/// 去掉一行里的 Markdown 链接：图片 `![说明](地址)` 整段去掉，链接 `[文字](地址)` 只留文字。
fn strip_links(line: &str) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < line.len() {
        let rest = &line[index..];
        if rest.starts_with("![") {
            if let Some((_, end)) = bracket_link(rest) {
                index += end;
                continue;
            }
        }
        if rest.starts_with('[') {
            if let Some((text, end)) = bracket_link(rest) {
                out.push_str(&text);
                index += end;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap_or(' ');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// `text` 以 `[` 开头且后面跟着 `(…)` 时，返回方括号里的文字和整个结构之后的下标。
fn bracket_link(text: &str) -> Option<(String, usize)> {
    let close = text.find(']')?;
    let after = &text[close + 1..];
    if !after.starts_with('(') {
        return None;
    }
    let paren = after.find(')')?;
    Some((text[1..close].to_string(), close + 1 + paren + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 GitHub Releases API 的字段形态构造一条 200 响应。
    /// （正文里有 Markdown 标题 `"## …"`，会提前结束 `r#"` 原始串，所以用三个井号。）
    fn release_json(tag: &str, assets: &str) -> String {
        format!(
            r###"{{"url":"https://api.github.com/repos/{REPO}/releases/1",
                "html_url":"https://github.com/{REPO}/releases/tag/{tag}",
                "tag_name":"{tag}","target_commitish":"main","name":"真终端与界面改版","draft":false,
                "prerelease":false,"created_at":"2026-11-01T00:00:00Z",
                "published_at":"2026-11-28T09:47:35Z","body":"## 更新内容\n- 真终端\n- [界面改版](https://example.com)",
                "assets":{assets}}}"###
        )
    }

    const ASSETS: &str = r#"[{"name":"SHA256SUMS.txt","browser_download_url":"https://example.com/SHA256SUMS.txt","size":10},
        {"name":"rabbitcontrol-win-v99.0.0.zip","browser_download_url":"https://example.com/rabbitcontrol-win-v99.0.0.zip","size":20}]"#;

    #[test]
    fn newer_release_is_reported_with_everything_the_dialog_shows() {
        let check = parse_response(&format!("STATUS:200\n{}", release_json("v99.0.0", ASSETS)))
            .expect("应解析成功");
        assert_eq!(check.latest.as_deref(), Some("99.0.0"));
        assert!(check.is_newer);
        assert!(check
            .status_text()
            .starts_with("新版本 99.0.0 可用：https://github.com/"));
        assert_eq!(check.title, "真终端与界面改版");
        assert_eq!(check.published_date(), "2026-11-28");
        assert_eq!(
            check.install_url(),
            "https://example.com/rabbitcontrol-win-v99.0.0.zip",
            "「立即更新」直奔安装包"
        );
        assert_eq!(
            check.plain_notes(),
            "【更新内容】\n• 真终端\n• 界面改版",
            "更新说明整理成纯文本"
        );
    }

    #[test]
    fn same_or_older_release_is_not_newer() {
        let same = parse_response(&format!("STATUS:200\n{}", release_json("v0.1.0", "[]")))
            .expect("应解析成功");
        assert!(!same.is_newer);
        assert_eq!(same.status_text(), "已是最新版本 0.1.0");
        assert_eq!(same.download_url, None);
        assert_eq!(
            same.install_url(),
            format!("https://github.com/{REPO}/releases/tag/v0.1.0").as_str(),
            "没有安装包就打开发布页面"
        );
    }

    #[test]
    fn other_zip_is_used_when_the_expected_name_is_missing() {
        let json = release_json(
            "v99.0.0",
            r#"[{"name":"notes.txt","browser_download_url":"https://example.com/notes.txt","size":1},
                {"name":"RabbitControl-other.zip","browser_download_url":"https://example.com/other.zip","size":2}]"#,
        );
        let check = parse_response(&format!("STATUS:200\n{json}")).expect("应解析成功");
        assert_eq!(
            check.download_url.as_deref(),
            Some("https://example.com/other.zip")
        );
    }

    #[test]
    fn no_release_yet_is_not_an_error() {
        let check =
            parse_response("STATUS:404\n{\"message\":\"Not Found\"}").expect("404 不是错误");
        assert_eq!(check.latest, None);
        assert!(!check.is_newer);
        assert_eq!(check.status_text(), "尚无发布版本（当前 0.1.0）");
        assert_eq!(check.published_date(), "");
        assert_eq!(
            check.install_url(),
            format!("https://github.com/{REPO}/releases").as_str()
        );
    }

    #[test]
    fn failures_are_distinguished() {
        assert_eq!(
            parse_response("STATUS:0\n").unwrap_err(),
            "无法连接 GitHub，请检查网络或代理设置"
        );
        assert_eq!(
            parse_response("STATUS:403\n{\"message\":\"API rate limit exceeded\"}").unwrap_err(),
            "GitHub API 请求过于频繁，请稍后再试"
        );
        assert_eq!(
            parse_response("STATUS:500\n").unwrap_err(),
            "GitHub API 返回 HTTP 500"
        );
        assert!(parse_response("garbage").is_err());
        assert!(parse_response("STATUS:200\nnot json").is_err());
    }

    #[test]
    fn current_version_is_comparable() {
        // 当前版本号必须能被 core 的版本比较消费
        assert!(!rc_client_core::updater::is_newer_version(
            "0.0.1",
            CURRENT_VERSION
        ));
        assert!(rc_client_core::updater::is_newer_version(
            "999.0.0",
            CURRENT_VERSION
        ));
    }

    #[test]
    fn notes_drop_markup_but_keep_code_and_plain_links() {
        let notes = "# 标题\n\n正文里有一个[链接](https://example.com/a)和一张图\n![截图](https://example.com/a.png)\n\n```\ncargo build\n```\n\n最后一行\n";
        assert_eq!(
            plain_notes(notes),
            "【标题】\n\n正文里有一个链接和一张图\n\ncargo build\n\n最后一行",
            "图片整行去掉、链接只留文字、代码块保留、空行最多一个"
        );
        assert_eq!(plain_notes(""), "");
    }

    #[test]
    fn long_notes_are_cut_on_a_character_boundary() {
        let long = "字".repeat(NOTES_LIMIT + 10);
        let cut = plain_notes(&long);
        assert_eq!(cut.chars().count(), NOTES_LIMIT + 1);
        assert!(cut.ends_with('…'));
    }

    /// 真实网络请求：手动验证用。
    #[test]
    #[ignore] // cargo test -p rc-ui check_live -- --ignored --nocapture
    fn check_live() {
        let result = check();
        println!("LIVE update check: {result:?}");
        let check = result.expect("真实请求应成功返回（包括 404 表示尚无 Release）");
        println!("LIVE status text: {}", check.status_text());
    }
}

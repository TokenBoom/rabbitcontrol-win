//! 自动更新检测（M6.3a）：查询 GitHub Releases 最新版。
//!
//! 通过 PowerShell `Invoke-WebRequest` 请求 GitHub API（系统代理直接生效，
//! 不给 rc-ui 增加 HTTP 客户端依赖），再用 core 的版本解析与比较。
//!
//! 当前只做「检测 + 提示」：下载与自替换未实现。
//!
//! 仓库没有任何 Release 时 GitHub 对 `releases/latest` 返回 404——这是正常
//! 状态而不是故障，文案上要和网络错误区分开。

#![deny(missing_docs)]

use std::process::Command;

/// 仓库标识（GitHub `owner/repo`）。
const REPO: &str = "TokenBoom/rabbitcontrol-win";
/// 当前版本（Cargo 包版本）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 单次请求超时（秒）。
const REQUEST_TIMEOUT_SECS: u32 = 20;

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
                url: format!("https://github.com/{REPO}/releases/tag/{}", info.tag),
                latest: Some(info.version),
            })
        }
        404 => Ok(UpdateCheck {
            latest: None,
            url: format!("https://github.com/{REPO}/releases"),
            is_newer: false,
        }),
        403 | 429 => Err("GitHub API 请求过于频繁，请稍后再试".into()),
        0 => Err("无法连接 GitHub，请检查网络或代理设置".into()),
        other => Err(format!("GitHub API 返回 HTTP {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 GitHub Releases API 的字段形态构造一条 200 响应。
    fn release_json(tag: &str) -> String {
        format!(
            r#"{{"url":"https://api.github.com/repos/{REPO}/releases/1",
                "html_url":"https://github.com/{REPO}/releases/tag/{tag}",
                "tag_name":"{tag}","target_commitish":"main","name":"v1","draft":false,
                "prerelease":false,"created_at":"2026-11-01T00:00:00Z",
                "published_at":"2026-11-01T00:00:00Z","body":"notes","assets":[]}}"#
        )
    }

    #[test]
    fn newer_release_is_reported() {
        let check = parse_response(&format!("STATUS:200\n{}", release_json("v99.0.0")))
            .expect("应解析成功");
        assert_eq!(check.latest.as_deref(), Some("99.0.0"));
        assert!(check.is_newer);
        assert!(check
            .status_text()
            .starts_with("新版本 99.0.0 可用：https://github.com/"));
    }

    #[test]
    fn same_or_older_release_is_not_newer() {
        let same =
            parse_response(&format!("STATUS:200\n{}", release_json("v0.1.0"))).expect("应解析成功");
        assert!(!same.is_newer);
        assert_eq!(same.status_text(), "已是最新版本 0.1.0");
    }

    #[test]
    fn no_release_yet_is_not_an_error() {
        let check =
            parse_response("STATUS:404\n{\"message\":\"Not Found\"}").expect("404 不是错误");
        assert_eq!(check.latest, None);
        assert!(!check.is_newer);
        assert_eq!(check.status_text(), "尚无发布版本（当前 0.1.0）");
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

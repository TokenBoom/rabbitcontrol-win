//! 自动更新检测（M6.3a）：GitHub Releases 最新版检查。
//!
//! 通过 PowerShell `Invoke-WebRequest` 获取 GitHub API 最新 Release JSON，
//! 用 core 的 `parse_github_release_json` 解析，`is_newer_version` 比较版本。
//! 避免给 rc-ui 添加 HTTP 客户端依赖。

/// 仓库标识（GitHub `owner/repo`）。
const REPO: &str = "TokenBoom/rabbitcontrol-win";
/// 当前版本（Cargo.toml `version.workspace`）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// GitHub Releases 最新版检查结果。
#[derive(Debug, Clone)]
pub struct UpdateCheck {
    /// 最新版本号（不含 `v` 前缀）。
    pub version: String,
    /// 发布 URL（暂未消费，后续接"下载更新"按钮）。
    #[allow(dead_code)]
    pub url: String,
    /// 是否比当前版本新。
    pub is_newer: bool,
    /// 更新日志（暂未消费，后续接"查看更新日志"链接）。
    #[allow(dead_code)]
    pub notes: String,
}

/// 检查 GitHub Releases 是否有新版本。
///
/// 通过 PowerShell 发起 HTTPS GET（系统代理自动生效）；阻塞 ≤30s，调用方
/// 须在工作线程执行。
pub fn check() -> Result<UpdateCheck, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let script = format!(
        "Invoke-WebRequest -Uri '{url}' -Headers @{{Accept='application/vnd.github+json'; \
         User-Agent='rabbitcontrol-win'}} -UseBasicParsing | Select-Object -ExpandProperty Content"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .map_err(|e| format!("更新检查进程启动失败：{e}"))?;
    if !output.status.success() {
        return Err("GitHub API 请求失败（网络不可达或限流）".into());
    }
    let json = String::from_utf8_lossy(&output.stdout);
    let info = rc_client_core::updater::parse_github_release_json(json.trim())
        .map_err(|e| format!("解析 Release 失败：{e}"))?;
    let is_newer = rc_client_core::updater::is_newer_version(&info.version, CURRENT_VERSION);
    Ok(UpdateCheck {
        version: info.version,
        url: format!("https://github.com/{REPO}/releases/tag/{}", info.tag),
        is_newer,
        notes: info.notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_parses_as_semver() {
        // 当前版本号必须能被 is_newer_version 消费（否则比较会 panic）
        assert!(!rc_client_core::updater::is_newer_version(
            "0.0.1",
            CURRENT_VERSION
        ));
    }
}

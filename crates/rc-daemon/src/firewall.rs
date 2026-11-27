//! 防火墙规则管理（M5b）：清理历史 hash 命名残留 + 注册稳定路径允许规则。
//!
//! 背景：应用每次重编译产生新二进制 hash，未签名时 Windows 防火墙按
//! `rabbitcontrol_win-<hash>` 命名弹窗，点「取消」还会落库为阻止规则，
//! 规则表越积越多、弹窗反复出现。本模块在 `setup`/`install`（提权态）执行：
//!
//! 1. PowerShell `Get-NetFirewallRule`（语言中立 cmdlet）按显示名通配清理
//!    历史自动规则（允许与阻止一并删除）；
//! 2. `netsh` 为本目录 `rc-daemon.exe` 与 `rabbitcontrol-win.exe` 添加
//!    入/出站允许规则（program 按路径匹配，重编译不失效）；
//! 3. 开发机兜底：exe 位于 `target\debug|release` 时额外注册另一
//!    profile 的稳定路径。

/// 需清理的历史自动规则显示名前缀（未签名 app 每次重编译产生）。
const STALE_RULE_PREFIX: &str = "rabbitcontrol_win-";
/// 本产品注册规则名前缀。
pub const RULE_PREFIX: &str = "RabbitControl";

/// 执行清理 + 注册；需提权态调用。
pub fn register() -> Result<(), String> {
    cleanup_stale()?;
    let targets = collect_targets()?;
    for path in &targets {
        add_rules_for(path)?;
    }
    Ok(())
}

/// 收集需要注册的可执行路径（含开发机另一 profile 兜底）。
fn collect_targets() -> Result<Vec<String>, String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .ok_or_else(|| "定位守护进程目录失败".to_string())?;
    let mut targets = Vec::new();
    for name in [
        "rc-daemon.exe",
        "rc-host-agent.exe",
        "rabbitcontrol-win.exe",
    ] {
        let path = exe_dir.join(name);
        if path.exists() {
            targets.push(path.to_string_lossy().into_owned());
        }
    }
    if let Some(profile) = exe_dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| *n == "debug" || *n == "release")
    {
        let other = if profile == "debug" {
            "release"
        } else {
            "debug"
        };
        if let Some(parent) = exe_dir.parent() {
            for name in [
                "rc-daemon.exe",
                "rc-host-agent.exe",
                "rabbitcontrol-win.exe",
            ] {
                let path = parent.join(other).join(name);
                if path.exists() {
                    targets.push(path.to_string_lossy().into_owned());
                }
            }
        }
    }
    if targets.is_empty() {
        return Err("未找到需注册的可执行文件".into());
    }
    Ok(targets)
}

/// 清理历史自动规则（含用户点「取消」落库的阻止规则）。
fn cleanup_stale() -> Result<(), String> {
    let script = format!(
        "Get-NetFirewallRule -DisplayName '{STALE_RULE_PREFIX}*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue"
    );
    run("powershell", ["-NoProfile", "-Command", &script])
}

/// 为单个可执行路径添加允许规则（入/出，协议 any）。
fn add_rules_for(image: &str) -> Result<(), String> {
    for dir in ["in", "out"] {
        run(
            "netsh",
            [
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={RULE_PREFIX} {dir} {image}"),
                &format!("dir={dir}"),
                "action=allow",
                &format!("program={image}"),
                "profile=any",
            ],
        )?;
    }
    Ok(())
}

/// 运行外部命令；非零退出视为失败（附 stderr）。
fn run<const N: usize>(exe: &str, args: [&str; N]) -> Result<(), String> {
    let output = std::process::Command::new(exe)
        .args(args)
        .output()
        .map_err(|e| format!("{exe} 启动失败：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("{exe} 退出非零：{}", stderr.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_prefix_is_hash_style() {
        // 用户反馈的弹窗来源：cargo deps 哈希命名
        assert!("rabbitcontrol_win-3099d1844bc5d0b5".starts_with(STALE_RULE_PREFIX));
    }
}

//! 后台被控进程的持久化开关（「允许被控」）。
//!
//! 与 core 的 `prefs.json` 同目录；临时文件 + 重命名原子写入。主界面与后台
//! 进程都可能写，读取一律容错（文件缺失或损坏按默认值处理）。
//!
//! 安全语义不变：被控仍需用户显式开启一次，之后重启 / 关窗保持。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 状态文件名（位于配置根目录）。
const STATE_FILE: &str = "agent.json";

/// 后台被控进程的持久化状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentState {
    /// 「允许被控」是否开启：开启后，登录状态下后台进程持续提供被控服务。
    pub host_enabled: bool,
}

impl AgentState {
    /// 状态文件路径。
    pub fn path() -> PathBuf {
        rc_client_core::app_prefs::default_prefs_path().with_file_name(STATE_FILE)
    }

    /// 从默认路径读取（容错）。
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// 写入默认路径。
    pub fn save(self) -> Result<(), String> {
        Self::save_to(&Self::path(), &self)
    }

    /// 从指定路径读取；缺失或损坏时返回默认值。
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// 原子写入指定路径。
    pub fn save_to(path: &Path, state: &Self) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败：{e}"))?;
        }
        let data = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &data).map_err(|e| format!("写入状态文件失败：{e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("替换状态文件失败：{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 每个测试一个自己的临时目录：默认线程数下两个测试并行跑，共用一个目录会互相删文件。
    fn state_dir(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rc-appcore-state-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_or_broken_file_means_disabled() {
        let dir = state_dir("broken");
        assert_eq!(
            AgentState::load_from(&dir.join("agent.json")),
            AgentState::default()
        );
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.json"), b"{ not json").unwrap();
        assert!(!AgentState::load_from(&dir.join("agent.json")).host_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_and_tolerant_unknown_fields() {
        let dir = state_dir("roundtrip");
        let path = dir.join("agent.json");
        AgentState::save_to(&path, &AgentState { host_enabled: true }).expect("保存");
        assert!(AgentState::load_from(&path).host_enabled);
        // 旧版本写出的文件缺少字段 / 多出未知字段：按默认值补齐，不报错
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(AgentState::load_from(&path), AgentState::default());
        std::fs::write(&path, b"{\"host_enabled\":true,\"future_field\":1}").unwrap();
        assert!(AgentState::load_from(&path).host_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

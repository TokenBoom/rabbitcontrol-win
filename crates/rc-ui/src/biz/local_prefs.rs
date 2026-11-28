//! 只属于主界面、core 的 `AppPrefs` 里没有的本地偏好。
//!
//! 与 `prefs.json` 同目录的 `ui-prefs.json`；读取一律容错（缺失或损坏按默认值处理），
//! 写入走临时文件 + 重命名。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 文件名（位于配置根目录）。
const FILE: &str = "ui-prefs.json";

/// 主界面自己的偏好（键名与 `prefs.json` 一样用 camelCase）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalPrefs {
    /// 启动时在后台检查更新；有新版本时右上角出现提醒。
    pub auto_check_update: bool,
}

impl Default for LocalPrefs {
    fn default() -> Self {
        Self {
            auto_check_update: true,
        }
    }
}

impl LocalPrefs {
    /// 文件路径。
    pub fn path() -> PathBuf {
        rc_client_core::app_prefs::default_prefs_path().with_file_name(FILE)
    }

    /// 从指定路径读取；缺失或损坏按默认值。
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// 写入指定路径（原子替换）。
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &data).map_err(|e| format!("写入界面偏好失败：{e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("替换界面偏好失败：{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_broken_file_means_defaults() {
        let dir = std::env::temp_dir().join(format!("rc-ui-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            LocalPrefs::load_from(&dir.join("none.json")),
            LocalPrefs::default()
        );
        assert!(
            LocalPrefs::default().auto_check_update,
            "默认要自动检查更新"
        );
        std::fs::write(dir.join("bad.json"), b"{ broken").unwrap();
        assert_eq!(
            LocalPrefs::load_from(&dir.join("bad.json")),
            LocalPrefs::default()
        );
        // 认识的字段照常读出来，多出来的未知字段不碍事
        std::fs::write(
            dir.join("extra.json"),
            br#"{"autoCheckUpdate":false,"future":1}"#,
        )
        .unwrap();
        assert!(!LocalPrefs::load_from(&dir.join("extra.json")).auto_check_update);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefs_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("rc-ui-prefs-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui-prefs.json");
        let prefs = LocalPrefs {
            auto_check_update: false,
        };
        prefs.save_to(&path).unwrap();
        assert_eq!(LocalPrefs::load_from(&path), prefs);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

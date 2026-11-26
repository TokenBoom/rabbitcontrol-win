//! 本地连接历史与收藏（纯客户端概念，服务端无对应模型）。
//!
//! 语义对齐 iOS `TargetHistory`：最近连接上限 12 条（按最后连接时间降序），
//! 收藏为独立集合；两者都以 9 位设备码为主键，**只存码与名称，不存密码**。
//! 持久化为 `%USERPROFILE%\.config\rabbitcontrol\history.json` 原子写。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 最近连接保留条数上限。
pub const MAX_RECENTS: usize = 12;

/// 一条最近连接记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentTarget {
    /// 9 位设备码（原始数字串）。
    pub device_id: String,
    /// 记录时的展示名（别名 > 主机名 > 设备码）。
    pub name: String,
    /// 最后连接时刻（unix 秒）。
    pub last_connected_at_sec: u64,
}

/// 连接历史存储。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetHistory {
    /// 最近连接（新在前，≤ [`MAX_RECENTS`]）。
    #[serde(default)]
    recents: Vec<RecentTarget>,
    /// 收藏设备码集合。
    #[serde(default)]
    favorites: Vec<String>,
}

impl TargetHistory {
    /// 从路径加载；缺失或损坏时返回空历史（不视为错误）。
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// 原子写入；目录不存在时自动创建。
    pub fn save_to(&self, path: &Path) -> std::result::Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| format!("创建配置目录失败：{err}"))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|err| err.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|err| format!("写入历史失败：{err}"))?;
        std::fs::rename(&tmp, path).map_err(|err| format!("提交历史失败：{err}"))?;
        Ok(())
    }

    /// 记录一次连接：置顶最近列表并裁剪到上限。
    pub fn record_connect(&mut self, device_id: &str, name: &str, at_sec: u64) {
        self.recents.retain(|item| item.device_id != device_id);
        self.recents.insert(
            0,
            RecentTarget {
                device_id: device_id.to_string(),
                name: name.to_string(),
                last_connected_at_sec: at_sec,
            },
        );
        self.recents.truncate(MAX_RECENTS);
    }

    /// 最近连接（新在前）。
    pub fn recents(&self) -> &[RecentTarget] {
        &self.recents
    }

    /// 是否已收藏。
    pub fn is_favorite(&self, device_id: &str) -> bool {
        self.favorites.iter().any(|item| item == device_id)
    }

    /// 切换收藏态，返回切换后的状态。
    pub fn toggle_favorite(&mut self, device_id: &str) -> bool {
        match self.favorites.iter().position(|item| item == device_id) {
            Some(index) => {
                self.favorites.remove(index);
                false
            }
            None => {
                self.favorites.push(device_id.to_string());
                true
            }
        }
    }

    /// 收藏设备码（按加入顺序）。
    // M1.4 收藏 tab 接线时移除该 allow。
    #[allow(dead_code)]
    pub fn favorites(&self) -> &[String] {
        &self.favorites
    }

    /// 移除一条最近记录（设备解绑时同步清理）。
    // M1.3 设备列表接线时移除该 allow。
    #[allow(dead_code)]
    pub fn remove_recent(&mut self, device_id: &str) {
        self.recents.retain(|item| item.device_id != device_id);
    }
}

/// 默认历史文件路径：`<用户配置目录>/history.json`。
// M1.4 接线时移除该 allow。
#[allow(dead_code)]
pub fn default_history_path() -> PathBuf {
    rc_client_core::config_paths::user_config_dir().join("history.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_connect_trims_and_orders() {
        let mut history = TargetHistory::default();
        for index in 0..(MAX_RECENTS + 3) {
            history.record_connect(
                &format!("{index:09}"),
                &format!("设备{index}"),
                index as u64,
            );
        }
        assert_eq!(history.recents().len(), MAX_RECENTS);
        // 新记录置顶，最旧的三条被裁剪。
        assert_eq!(
            history.recents()[0].device_id,
            format!("{:09}", MAX_RECENTS + 2)
        );
        assert!(!history
            .recents()
            .iter()
            .any(|item| item.device_id == "000000000"));
    }

    #[test]
    fn favorites_toggle_roundtrip() {
        let mut history = TargetHistory::default();
        assert!(!history.is_favorite("123456789"));
        assert!(history.toggle_favorite("123456789"));
        assert!(history.is_favorite("123456789"));
        assert!(!history.toggle_favorite("123456789"));
        assert!(!history.is_favorite("123456789"));
    }

    #[test]
    fn json_roundtrip_preserves_state() {
        let mut history = TargetHistory::default();
        history.record_connect("123456789", "测试机", 1000);
        history.record_connect("987654321", "笔记本", 2000);
        history.toggle_favorite("123456789");

        let dir = std::env::temp_dir().join(format!("rc-win-history-test-{}", std::process::id()));
        let path = dir.join("history.json");
        history.save_to(&path).expect("保存失败");

        let loaded = TargetHistory::load_from(&path);
        assert_eq!(loaded, history);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_yields_empty() {
        let history = TargetHistory::load_from(Path::new("Z:/不存在的目录/history.json"));
        assert_eq!(history, TargetHistory::default());
    }
}

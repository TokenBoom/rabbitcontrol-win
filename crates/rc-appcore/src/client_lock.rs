//! 客户端安全锁定（对齐 UU 远程的「安全锁定」；语义同 iOS / macOS 的 `client_lock`）。
//!
//! 锁定期间本地不能修改客户端设置、也不能退出客户端；发起远程连接不受影响。
//! 解锁要输入安全密钥。用途：有人正在远程协助这台机器时，防止对方改动验证码、
//! 验证方式这类敏感设置。
//!
//! - 安全密钥只存「随机盐 + PBKDF2-HMAC-SHA256 派生值」，不存明文，保存后不可查看；
//! - 按登录账号分别保存；手动锁定的状态落盘，重启后仍在锁定中；
//! - 连续输错有冷却，避免在界面上暴力试密钥。
//!
//! GUI-free，可单测。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pbkdf2::pbkdf2_hmac;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// 状态文件名（与 `prefs.json` 同目录）。
const STATE_FILE: &str = "client-lock.json";
/// PBKDF2 轮数（OWASP 对 PBKDF2-HMAC-SHA256 的建议量级）。
const ROUNDS: u32 = 210_000;
const SALT_LEN: usize = 16;
const DIGEST_LEN: usize = 32;

/// 安全密钥最短长度。
pub const KEY_MIN_LEN: usize = 8;
/// 安全密钥最长长度。
pub const KEY_MAX_LEN: usize = 16;
/// 连续输错这么多次之后开始冷却。
pub const MAX_FAILURES: u32 = 5;

/// 安全密钥的校验数据（不是密钥本身）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verifier {
    /// 16 字节随机盐的十六进制。
    pub salt_hex: String,
    /// PBKDF2 派生值的十六进制。
    pub digest_hex: String,
}

/// 一个账号的安全锁定配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountLock {
    /// 设过安全密钥才有；没有就不能锁定。
    pub verifier: Option<Verifier>,
    /// 手动锁定过、还没解锁（落盘，重启后保持）。自动锁定不写这里。
    pub locked: bool,
    /// 远程协助本设备期间自动锁定（协助结束自动解锁）。
    pub auto_lock_on_assist: bool,
    /// 启动客户端时自动锁定。
    pub auto_lock_on_start: bool,
}

/// 全部账号的安全锁定配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LockStore {
    accounts: BTreeMap<String, AccountLock>,
}

impl LockStore {
    /// 状态文件路径。
    pub fn path() -> PathBuf {
        rc_client_core::app_prefs::default_prefs_path().with_file_name(STATE_FILE)
    }

    /// 从默认路径读取（容错）。
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// 从指定路径读取；缺失或损坏按「没有配置」处理——文件坏了不应该把人锁在外面。
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// 写入指定路径：临时文件 + 重命名，原子替换。
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &data).map_err(|e| format!("写入安全锁定配置失败：{e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("替换安全锁定配置失败：{e}"))
    }

    /// 某个账号的配置（没配置过就是全默认）。
    pub fn account(&self, user_id: &str) -> AccountLock {
        self.accounts.get(user_id).cloned().unwrap_or_default()
    }

    /// 改某个账号的配置。
    pub fn update(&mut self, user_id: &str, change: impl FnOnce(&mut AccountLock)) {
        change(self.accounts.entry(user_id.to_string()).or_default());
    }
}

/// 安全密钥的长度要求（8–16 位；首尾空白不算）。
pub fn validate_key(key: &str) -> Result<(), String> {
    let length = key.trim().chars().count();
    if (KEY_MIN_LEN..=KEY_MAX_LEN).contains(&length) {
        Ok(())
    } else {
        Err(format!("安全密钥需为 {KEY_MIN_LEN}–{KEY_MAX_LEN} 位"))
    }
}

/// 由密钥生成校验数据（随机盐）。
pub fn make_verifier(key: &str) -> Result<Verifier, String> {
    validate_key(key)?;
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).map_err(|e| format!("生成随机盐失败：{e}"))?;
    let salt_hex = to_hex(&salt);
    let digest_hex = to_hex(&derive(key.trim(), &salt, ROUNDS));
    Ok(Verifier {
        salt_hex,
        digest_hex,
    })
}

/// 密钥是否正确（常量时间比较）。
pub fn verify(verifier: &Verifier, key: &str) -> bool {
    match from_hex(&verifier.salt_hex) {
        Some(salt) => constant_time_eq(&derive(key.trim(), &salt, ROUNDS), &verifier.digest_hex),
        None => false,
    }
}

fn derive(key: &str, salt: &[u8], rounds: u32) -> [u8; DIGEST_LEN] {
    let mut out = [0u8; DIGEST_LEN];
    pbkdf2_hmac::<Sha256>(key.as_bytes(), salt, rounds, &mut out);
    out
}

/// 逐字节比较，不因第一个不同的位置提前返回。
fn constant_time_eq(a: &[u8], b_hex: &str) -> bool {
    match from_hex(b_hex) {
        Some(b) if a.len() == b.len() => {
            a.iter().zip(&b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
        }
        _ => false,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

/// 解锁的失败计数与冷却。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnlockAttempts {
    failed: u32,
    cooldown_until_sec: u64,
}

impl UnlockAttempts {
    /// 新的一轮尝试（解锁成功后也回到这里）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 已经连续输错的次数。
    pub fn failed(&self) -> u32 {
        self.failed
    }

    /// 冷却还剩多少秒（0 = 现在就可以再试）。
    pub fn cooldown_left(&self, now_sec: u64) -> u64 {
        self.cooldown_until_sec.saturating_sub(now_sec)
    }

    /// 记一次输错；返回这次的冷却秒数（0 = 还没到需要冷却的次数）。
    ///
    /// 第 5 次起冷却 5 秒，之后每再错一次翻倍：10 / 20 / 40 / 60，封顶 60 秒。
    pub fn record_failure(&mut self, now_sec: u64) -> u32 {
        self.failed = self.failed.saturating_add(1);
        if self.failed < MAX_FAILURES {
            return 0;
        }
        let steps = (self.failed - MAX_FAILURES).min(4);
        let penalty = (5u32 << steps).min(60);
        self.cooldown_until_sec = now_sec + u64::from(penalty);
        penalty
    }

    /// 清零（解锁成功 / 换账号）。
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_dir(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rc-appcore-lock-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_length_is_checked_on_the_trimmed_value() {
        assert!(validate_key("abcd1234").is_ok());
        assert!(validate_key("  abcd1234  ").is_ok(), "首尾空白不算长度");
        assert!(validate_key("abcd123").is_err());
        assert!(validate_key("abcdefghijklmnopq").is_err());
        assert!(validate_key("abcdefghijklmnop").is_ok());
    }

    #[test]
    fn verifier_round_trips_with_a_fresh_salt_each_time() {
        let first = make_verifier("abcd1234").unwrap();
        let second = make_verifier("abcd1234").unwrap();
        assert_ne!(first.salt_hex, second.salt_hex, "每次生成都要换盐");
        assert_eq!(first.salt_hex.len(), 32);
        assert_eq!(first.digest_hex.len(), 64);
        assert!(
            !serde_json::to_string(&first).unwrap().contains("abcd1234"),
            "不存明文"
        );
        assert!(verify(&first, "abcd1234"));
        assert!(verify(&first, "  abcd1234 "), "首尾空白不影响校验");
        assert!(!verify(&first, "abcd12345"));
        assert!(!verify(&first, ""));
    }

    #[test]
    fn broken_verifier_never_unlocks() {
        let bad_salt = Verifier {
            salt_hex: "zz".into(),
            digest_hex: "00".repeat(32),
        };
        assert!(!verify(&bad_salt, "abcd1234"));
        let short = Verifier {
            salt_hex: "00".repeat(16),
            digest_hex: "00".into(),
        };
        assert!(!verify(&short, "abcd1234"));
    }

    #[test]
    fn store_keeps_accounts_apart_and_survives_a_rewrite() {
        let dir = store_dir("store");
        let path = dir.join("client-lock.json");
        let mut store = LockStore::default();
        assert_eq!(
            store.account("u1"),
            AccountLock::default(),
            "没配置过的账号是全默认"
        );
        let verifier = make_verifier("abcd1234").unwrap();
        store.update("u1", |lock| {
            lock.verifier = Some(verifier.clone());
            lock.locked = true;
            lock.auto_lock_on_start = true;
        });
        store.save_to(&path).unwrap();
        let mut loaded = LockStore::load_from(&path);
        assert!(loaded.account("u1").locked);
        assert!(loaded.account("u1").auto_lock_on_start);
        assert_eq!(
            loaded.account("u2"),
            AccountLock::default(),
            "账号之间互不影响"
        );
        loaded.update("u1", |lock| lock.locked = false);
        loaded.save_to(&path).unwrap();
        let again = LockStore::load_from(&path).account("u1");
        assert!(!again.locked, "写入要真的落盘");
        assert!(again.verifier.is_some(), "解锁不清密钥");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_broken_file_is_not_locked() {
        let dir = store_dir("broken");
        assert_eq!(
            LockStore::load_from(&dir.join("none.json")),
            LockStore::default()
        );
        std::fs::write(dir.join("bad.json"), b"{ not json").unwrap();
        assert_eq!(
            LockStore::load_from(&dir.join("bad.json")),
            LockStore::default()
        );
        // 认识的字段照常读出来，多出来的未知字段不碍事
        std::fs::write(
            dir.join("extra.json"),
            br#"{"accounts":{"u1":{"locked":true,"future":1}}}"#,
        )
        .unwrap();
        assert!(
            LockStore::load_from(&dir.join("extra.json"))
                .account("u1")
                .locked
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_cool_down_and_reset() {
        let mut attempts = UnlockAttempts::new();
        for now in 0..4 {
            assert_eq!(attempts.record_failure(now), 0, "前 4 次不冷却");
            assert_eq!(attempts.cooldown_left(now), 0);
        }
        assert_eq!(attempts.record_failure(100), 5);
        assert_eq!(attempts.cooldown_left(102), 3);
        assert_eq!(attempts.cooldown_left(105), 0);
        assert_eq!(attempts.record_failure(200), 10, "再错一次冷却翻倍");
        assert_eq!(attempts.record_failure(300), 20);
        assert_eq!(attempts.record_failure(400), 40);
        assert_eq!(attempts.record_failure(500), 60, "封顶 60 秒");
        assert_eq!(attempts.record_failure(600), 60);
        assert_eq!(attempts.failed(), 10, "计数不因封顶而停住");
        attempts.reset();
        assert_eq!(attempts, UnlockAttempts::new());
    }

    #[test]
    fn pbkdf2_matches_the_published_test_vector() {
        // PBKDF2-HMAC-SHA256 的公开测试向量：password / salt / 1 轮
        let out = derive("password", b"salt", 1);
        assert_eq!(
            to_hex(&out),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
    }
}

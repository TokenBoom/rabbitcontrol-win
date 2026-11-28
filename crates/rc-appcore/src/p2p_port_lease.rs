//! 跨进程 P2P UDP 端口租约（core v0.2.7 的 `ExternalUdpPortHoldHook` 在 Windows 上的实装）。
//!
//! P2P 打洞必须从信令注册时用的那个本地 UDP 端口发包（服务端只知道那个端口的 NAT
//! 映射），而且要独占它。信令客户端在持有它的进程里留了一个常驻探测套接字；打洞若发生在
//! **另一个进程**里，就得请持有方临时松手，打洞结束再恢复探测。主界面与后台进程谁在托管
//! 信令是动态的，所以：
//!
//! - 持有探测端口的一方（一般是后台被控进程）维护一张租约表 [`P2pPortLeases`]，经管道
//!   应答申请 / 续租 / 归还（协议见 [`crate::agent_ipc::Request`]）；
//! - 发起打洞的一方（主界面）用 [`register_hook`] 向 core 注册钩子：本进程没持有该端口时，
//!   经管道向对端申请租约；守卫 [`LeaseGuard`] 存续期间每 10 秒续一次租（租期 30 秒），
//!   `Drop` 时归还。
//!
//! 对端没在运行、或它也没持有该端口时，钩子返回 `None`，core 按没有租约继续（与不注册
//! 钩子时行为一致）。

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rc_client_core::signal_client::{
    acquire_local_udp_port_for_p2p, set_external_udp_port_hold_hook, LocalUdpPortHoldGuard,
};

use crate::agent_ipc::{self, Request};

/// 租约时长（毫秒）。
pub const LEASE_TTL_MS: u64 = 30_000;
/// 续租间隔。
pub const RENEW_INTERVAL: Duration = Duration::from_secs(10);
/// 单次管道请求的超时（毫秒）：打洞路径在等它，不能久等。
pub const IPC_TIMEOUT_MS: u32 = 500;
/// 同时存在的租约上限（防失控的调用方把表撑爆）。
pub const MAX_LEASES: usize = 16;
/// 租约时长下限 / 上限。
const TTL_MIN: Duration = Duration::from_millis(5_000);
const TTL_MAX: Duration = Duration::from_millis(60_000);

/// 租约时长规整：0 取默认 30 秒，其余夹到 5–60 秒（对端失联时探测端口最多被占 60 秒）。
fn clamp_ttl(lease_ttl_ms: u64) -> Duration {
    if lease_ttl_ms == 0 {
        Duration::from_millis(LEASE_TTL_MS)
    } else {
        Duration::from_millis(lease_ttl_ms).clamp(TTL_MIN, TTL_MAX)
    }
}

/// 一条租约：持有期间探测套接字保持释放状态（`_hold` 落地即恢复探测）。
struct Lease {
    port: u16,
    _hold: LocalUdpPortHoldGuard,
    expires_at: Instant,
}

/// 持有探测端口一方维护的租约表。
///
/// 过期租约不靠定时器回收：每次申请 / 续租和持有方的例行巡检 [`Self::prune_expired`] 时
/// 清理；租约过期即恢复探测。
#[derive(Default)]
pub struct P2pPortLeases {
    next_id: u64,
    leases: HashMap<u64, Lease>,
}

impl P2pPortLeases {
    /// 申请租约：先在本进程让出该端口的探测套接字，再登记一条会到期的租约，返回租约号。
    pub fn acquire(&mut self, port: u16, lease_ttl_ms: u64) -> Result<u64, String> {
        let hold = acquire_local_udp_port_for_p2p(Some(port));
        let holding = hold.is_holding_local_slot();
        self.insert(port, lease_ttl_ms, hold, holding)
    }

    /// 登记租约（[`Self::acquire`] 的可测内核：`holding` = `hold` 是否真的让出了本进程的
    /// 探测槽位；测试用假守卫 + 自定过期时间走完整张表）。
    fn insert(
        &mut self,
        port: u16,
        lease_ttl_ms: u64,
        hold: LocalUdpPortHoldGuard,
        holding: bool,
    ) -> Result<u64, String> {
        self.insert_expiring_at(
            port,
            hold,
            holding,
            Instant::now() + clamp_ttl(lease_ttl_ms),
        )
    }

    fn insert_expiring_at(
        &mut self,
        port: u16,
        hold: LocalUdpPortHoldGuard,
        holding: bool,
        expires_at: Instant,
    ) -> Result<u64, String> {
        self.prune_expired();
        if port == 0 {
            return Err("无效 UDP 端口号 0".to_string());
        }
        if self.leases.len() >= MAX_LEASES {
            return Err(format!("跨进程 P2P UDP 端口租约数已达上限（{MAX_LEASES}）"));
        }
        if !holding {
            return Err(format!("后台进程当前未持有 UDP 端口 {port} 的探测槽位"));
        }
        // 租约号从 1 起，回绕后跳过仍在使用的号
        let mut lease_id = self.next_id.max(1);
        while self.leases.contains_key(&lease_id) {
            lease_id = lease_id.wrapping_add(1).max(1);
        }
        self.next_id = lease_id.wrapping_add(1).max(1);
        self.leases.insert(
            lease_id,
            Lease {
                port,
                _hold: hold,
                expires_at,
            },
        );
        Ok(lease_id)
    }

    /// 续租：把到期时间往后推。租约不存在或已过期报错（调用方应视为已失去租约）。
    pub fn renew(&mut self, lease_id: u64, lease_ttl_ms: u64) -> Result<(), String> {
        self.prune_expired();
        let expires_at = Instant::now() + clamp_ttl(lease_ttl_ms);
        let lease = self
            .leases
            .get_mut(&lease_id)
            .ok_or_else(|| format!("UDP 端口租约 {lease_id} 不存在或已过期"))?;
        lease.expires_at = expires_at;
        Ok(())
    }

    /// 归还租约（立即恢复该端口的探测）。不存在的租约号按已归还处理。
    pub fn release(&mut self, lease_id: u64) {
        self.leases.remove(&lease_id);
    }

    /// 清掉过期租约，返回这次清掉的（租约号, 端口），供调用方记日志。
    pub fn prune_expired(&mut self) -> Vec<(u64, u16)> {
        let now = Instant::now();
        let expired: Vec<(u64, u16)> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.expires_at <= now)
            .map(|(id, lease)| (*id, lease.port))
            .collect();
        for (id, _) in &expired {
            self.leases.remove(id);
        }
        expired
    }

    /// 清空全部租约（信令停止、进程让位时：探测套接字都没了，租约无从谈起）。
    pub fn clear(&mut self) {
        self.leases.clear();
    }

    /// 当前租约数。
    pub fn len(&self) -> usize {
        self.leases.len()
    }

    /// 没有任何租约。
    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }
}

/// 发起打洞一方持有的租约守卫：存续期间定期续租，`Drop` 时归还。
///
/// core 把它原样存进 `LocalUdpPortHoldGuard`，在 P2P 连接结束（或打洞失败）时释放。
struct LeaseGuard {
    lease_id: u64,
    stop: Arc<(Mutex<bool>, Condvar)>,
    renew_thread: Option<std::thread::JoinHandle<()>>,
}

impl LeaseGuard {
    fn spawn(lease_id: u64) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_stop = Arc::clone(&stop);
        let renew_thread = std::thread::Builder::new()
            .name(format!("rc-udp-lease-{lease_id}"))
            .spawn(move || {
                loop {
                    let (lock, signal) = &*thread_stop;
                    let stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if *stopped {
                        break;
                    }
                    let (stopped, waited) = signal
                        .wait_timeout(stopped, RENEW_INTERVAL)
                        .unwrap_or_else(|e| e.into_inner());
                    if *stopped {
                        break;
                    }
                    drop(stopped);
                    if waited.timed_out() {
                        // 续不上（对端已退出）就不再续：租约到期后对端自会恢复探测
                        let renewed = agent_ipc::call(
                            &Request::RenewP2pUdpPort {
                                lease_id,
                                lease_ttl_ms: LEASE_TTL_MS,
                            },
                            IPC_TIMEOUT_MS,
                        )
                        .map(|response| response.ok);
                        if renewed != Ok(true) {
                            break;
                        }
                    }
                }
            })
            .ok();
        Self {
            lease_id,
            stop,
            renew_thread,
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        {
            let (lock, signal) = &*self.stop;
            let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
            *stopped = true;
            signal.notify_all();
        }
        if let Some(thread) = self.renew_thread.take() {
            let _ = thread.join();
        }
        let _ = agent_ipc::call(
            &Request::ReleaseP2pUdpPort {
                lease_id: self.lease_id,
            },
            IPC_TIMEOUT_MS,
        );
    }
}

/// 向对端申请一条租约；对端没在运行、拒绝或超时都返回 `None`。
fn acquire_lease(port: u16) -> Option<LeaseGuard> {
    if port == 0 {
        return None;
    }
    match agent_ipc::call(
        &Request::AcquireP2pUdpPort {
            port,
            lease_ttl_ms: LEASE_TTL_MS,
        },
        IPC_TIMEOUT_MS,
    ) {
        Ok(response) if response.ok => response
            .get::<u64>("lease_id")
            .filter(|id| *id != 0)
            .map(LeaseGuard::spawn),
        Ok(response) => {
            eprintln!(
                "[p2p] 跨进程 UDP 端口租约被拒（端口 {port}）：{}",
                response.error.unwrap_or_default()
            );
            None
        }
        Err(err) => {
            eprintln!("[p2p] 跨进程 UDP 端口租约申请失败（端口 {port}）：{err}");
            None
        }
    }
}

/// 向 core 注册跨进程 UDP 端口租约钩子（发起 P2P 打洞的进程在启动时调用一次）。
///
/// 只在本进程没持有该端口的探测槽位时才会被 core 调用；本进程自己持有则 core 直接用，
/// 不走这里。
pub fn register_hook() {
    set_external_udp_port_hold_hook(Some(Arc::new(|port| {
        acquire_lease(port).map(|guard| Box::new(guard) as Box<dyn Any + Send + Sync>)
    })));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没有命中任何探测槽位的守卫（不开任何套接字）。
    fn empty_hold() -> LocalUdpPortHoldGuard {
        acquire_local_udp_port_for_p2p(None)
    }

    #[test]
    fn acquire_rejects_port_zero_and_unheld_ports() {
        let mut leases = P2pPortLeases::default();
        let err = leases.insert(0, 0, empty_hold(), false).unwrap_err();
        assert_eq!(err, "无效 UDP 端口号 0");
        let err = leases.insert(44350, 0, empty_hold(), false).unwrap_err();
        assert_eq!(err, "后台进程当前未持有 UDP 端口 44350 的探测槽位");
        assert!(leases.is_empty());
    }

    #[test]
    fn ttl_is_clamped() {
        assert_eq!(clamp_ttl(0), Duration::from_millis(LEASE_TTL_MS));
        assert_eq!(clamp_ttl(1), TTL_MIN);
        assert_eq!(clamp_ttl(20_000), Duration::from_millis(20_000));
        assert_eq!(clamp_ttl(u64::MAX), TTL_MAX);
    }

    #[test]
    fn leases_get_unique_ids_and_are_capped() {
        let mut leases = P2pPortLeases::default();
        let soon = Instant::now() + Duration::from_secs(60);
        let mut ids = Vec::new();
        for _ in 0..MAX_LEASES {
            ids.push(
                leases
                    .insert_expiring_at(44350, empty_hold(), true, soon)
                    .unwrap(),
            );
        }
        assert_eq!(ids[0], 1);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MAX_LEASES, "租约号不能重复");
        // 同一个端口可以有多条租约（多个在途打洞），但总数封顶
        let err = leases
            .insert_expiring_at(44350, empty_hold(), true, soon)
            .unwrap_err();
        assert!(err.contains("已达上限"), "{err}");
        assert_eq!(leases.len(), MAX_LEASES);
    }

    #[test]
    fn renew_release_and_expiry_manage_the_table() {
        let mut leases = P2pPortLeases::default();
        let soon = Instant::now() + Duration::from_secs(60);
        let id = leases
            .insert_expiring_at(40000, empty_hold(), true, soon)
            .unwrap();
        // 续租推进到期时间；归还后消失
        leases.renew(id, 60_000).unwrap();
        assert_eq!(leases.len(), 1);
        leases.release(id);
        assert!(leases.is_empty());
        assert_eq!(
            leases.renew(id, 0).unwrap_err(),
            "UDP 端口租约 1 不存在或已过期"
        );
        // 过期的租约在下次巡检 / 续租 / 申请时被清掉，没过期的留着
        let soon = Instant::now() + Duration::from_secs(60);
        let past = Instant::now() - Duration::from_secs(1);
        leases
            .insert_expiring_at(40002, empty_hold(), true, soon)
            .unwrap();
        let stale = leases
            .insert_expiring_at(40001, empty_hold(), true, past)
            .unwrap();
        assert_eq!(leases.len(), 2);
        assert_eq!(leases.prune_expired(), vec![(stale, 40001)]);
        assert_eq!(leases.len(), 1);
        leases.clear();
        assert!(leases.is_empty());
    }
}

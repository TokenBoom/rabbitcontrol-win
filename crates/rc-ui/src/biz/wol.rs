//! WOL 唤醒集成（#7）：设备列表离线设备「远程唤醒」按钮。
use rc_client_core::wol;

/// 向目标设备发送 WOL magic packet。
/// `mac_addr` 为设备 MAC 地址字符串，`lan_addrs` 为已知 LAN 地址列表。
#[allow(dead_code)] // UI 接线待下批
pub fn wake(mac_addr: &str, lan_addrs: &[String]) -> Result<usize, String> {
    wol::send_magic_packet(mac_addr, lan_addrs)
}

//! `loopback` 子命令：进程内采集→编码→解码→呈现全链压测（M5a.8）。
//!
//! 走真实平台后端（DXGI 采集 → MF H.264 编码 → MF 解码 → D3D11 渲染），
//! 用 core 的 [`Loopback`] 驱动与报告产出。

use std::time::Duration;

use rc_loopback::{Loopback, LoopbackConfig};
use rc_types::Resolution;

/// 运行真实后端回环压测。
pub fn run(duration_s: u64, width: u32, height: u32, fps: u32, strict: bool) -> Result<(), String> {
    let cfg = LoopbackConfig {
        resolution: Resolution::new(width, height),
        fps,
        ..LoopbackConfig::default()
    };
    println!(
        "== 回环压测（真实后端）==\n分辨率 {}x{} @ {} fps · 时长 {} s · 严格模式 {}",
        width, height, fps, duration_s, strict
    );
    let mut lb = Loopback::new(cfg).map_err(|e| format!("回环构造失败：{e}"))?;
    let report = lb
        .run_real(Duration::from_secs(duration_s), true, strict)
        .map_err(|e| format!("回环运行失败：{e}"))?;
    println!("\n== 报告 ==");
    println!("{report:#?}");
    Ok(())
}

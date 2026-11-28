//! `capture-probe` 子命令：单独验证 DXGI 桌面复制能不能产出画面帧（不经过会话管线）。
//!
//! 会话里「没有画面」时用它区分两种情况：采集本身就不出帧（这里帧数为 0），还是采集正常、
//! 问题出在管线 / 编码 / 传输。

use std::time::{Duration, Instant};

use rc_capture::CaptureBackend;
use rc_capture_win::{new_backend, DxgiConfig};

/// 运行采集探测。`settle_s`：创建 duplication 之后、开始取帧之前先静置的秒数
/// （用来验证「首帧是不是只在创建后立刻去取才拿得到」）。
pub fn run(duration_s: u64, settle_s: u64) -> Result<(), String> {
    let mut backend = new_backend(DxgiConfig {
        display: 0,
        fps: 60,
        resolution: None,
        origin_us: None,
    })
    .map_err(|err| format!("DXGI 采集初始化失败：{err}"))?;
    println!("== DXGI 采集探测 ==\n显示器 0 · 时长 {duration_s} s（期间请让屏幕内容有变化）");
    if settle_s > 0 {
        println!("duplication 已创建，静置 {settle_s} s 后再开始取帧 …");
        std::thread::sleep(Duration::from_secs(settle_s));
    }
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let (mut frames, mut idle, mut first_frame_ms) = (0u32, 0u32, 0u32);
    let started = Instant::now();
    while Instant::now() < deadline {
        match backend.next_frame(Duration::from_millis(100)) {
            Ok(Some(_)) => {
                if frames == 0 {
                    first_frame_ms = started.elapsed().as_millis() as u32;
                }
                frames += 1;
            }
            Ok(None) => idle += 1,
            Err(err) => return Err(format!("采集出错：{err}")),
        }
        if backend.poll_invalidation().is_some() {
            return Err("采集中途失效（锁屏 / 分辨率变化 / 桌面切换）".to_string());
        }
    }
    println!("帧数：{frames}（空转 {idle} 次，首帧在 {first_frame_ms} ms）");
    if frames == 0 {
        return Err("探测期间没有采到任何画面更新".to_string());
    }
    Ok(())
}

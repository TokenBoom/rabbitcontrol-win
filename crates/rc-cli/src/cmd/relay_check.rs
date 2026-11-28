//! `relay-check` 子命令：验证桌面采集的跨设备中转链路。
//!
//! 桌面采集在独立的 D3D11 设备上，帧经共享纹理搬到全进程共享设备。被控端「采到帧但编码器报错 / 画面不对」时先跑它：
//! 不依赖桌面内容，锁屏时也能跑。

/// 纯灰 (128, 128, 128) 经 BT.709 有限范围转换后的期望值与允许误差。
const EXPECTED_LUMA: i32 = 126;
const EXPECTED_CHROMA: i32 = 128;
const TOLERANCE: i32 = 6;

/// 运行中转链路自检；像素对得上才算通过。
pub fn run() -> Result<(), String> {
    rc_platform_win::register_all();
    println!("== 桌面采集中转链路自检 ==");
    let (luma, chroma) = rc_capture_win::relay_self_check()?;
    println!(
        "纯灰 128 经「采集专用设备 → 共享纹理 → 共享设备 → NV12」后读回：亮度 {luma}（期望 {EXPECTED_LUMA}）· U {chroma}（期望 {EXPECTED_CHROMA}）"
    );
    if (luma as i32 - EXPECTED_LUMA).abs() > TOLERANCE
        || (chroma as i32 - EXPECTED_CHROMA).abs() > TOLERANCE
    {
        return Err("中转后的像素与写入值对不上（允许误差 ±6）".to_string());
    }
    println!("通过：中转链路正常");
    Ok(())
}

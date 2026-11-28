//! `conv-bench` 子命令：实测 BGRA → NV12 转换（含缩放）的 GPU 耗时。
//!
//! 被控端每次出帧都要把桌面从 BGRA 转成编码器要的 NV12。帧率上不去、编码总是超时的时候，
//! 用它确认是不是这一步拖慢了后面的 GPU 工作：

/// 运行转换计时。输入是桌面分辨率，输出是编码分辨率；打印每次转换的 GPU 耗时。
pub fn run(
    width: u32,
    height: u32,
    out_width: u32,
    out_height: u32,
    iterations: u32,
) -> Result<(), String> {
    rc_platform_win::register_all();
    let iterations = iterations.clamp(1, 600);
    println!(
        "== 色彩转换计时 ==\n{width}x{height} → {out_width}x{out_height} · 连做 {iterations} 次取平均"
    );
    let (batched_ms, single_ms, baseline_ms) =
        rc_capture_win::conversion_gpu_ms((width, height), (out_width, out_height), iterations)
            .map_err(|err| format!("转换计时失败：{err}"))?;
    println!(
        "每次转换：连做 {batched_ms:.2} ms · 逐次等 GPU 做完 {single_ms:.2} ms（空载基线 {baseline_ms:.2} ms）"
    );
    Ok(())
}

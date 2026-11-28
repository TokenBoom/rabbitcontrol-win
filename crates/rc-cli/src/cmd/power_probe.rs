//! `power-probe` 子命令：验证被控端依赖的三件事——显示器熄屏通知、熄屏唤醒、锁屏查询。
//!
//! 被控端「连接后一直没有画面」时用它确认：显示器是不是熄屏了（熄屏后 DXGI 桌面
//! 复制不出帧）、唤醒是否生效、会话有没有被判成锁屏（锁屏时采集同样不出帧）。

use std::time::{Duration, Instant};

use rc_win32core::power::{display_power, wake_display_if_asleep, DisplayPower, PowerRequest};

/// 运行探测。`wake`：若显示器熄屏则执行唤醒并持有守卫；`hold`：无论熄屏与否都持有
/// 「显示器常亮 + 系统不眠」的电源请求，用来验证会话期间的保活。
pub fn run(duration_s: u64, wake: bool, hold: bool) -> Result<(), String> {
    let started = Instant::now();
    let locked = rc_win32core::screen_is_locked();
    let power = display_power();
    let known_ms = started.elapsed().as_millis();
    println!("== 电源与桌面状态探测 ==\n时长 {duration_s} s");
    println!("初始：锁屏 {locked} · 显示器 {power:?}（{known_ms} ms 内取得）");

    let mut wake_guard = None;
    if wake {
        let woke_at = Instant::now();
        match wake_display_if_asleep(
            "RabbitControl 诊断：power-probe 唤醒显示器",
            Duration::from_millis(2500),
        ) {
            Ok(Some(guard)) => {
                println!(
                    "唤醒：显示器原本熄屏，{} ms 后亮屏（守卫持有中）",
                    woke_at.elapsed().as_millis()
                );
                wake_guard = Some(guard);
            }
            Ok(None) => println!("唤醒：显示器本来就亮，无需唤醒"),
            Err(err) => println!("唤醒：失败：{err}"),
        }
    }
    let hold_guard =
        hold.then(|| PowerRequest::begin("RabbitControl 诊断：power-probe 保活", true));
    match &hold_guard {
        Some(Ok(guard)) => println!(
            "保活：电源请求已持有（显示器常亮 {}）",
            guard.keeps_display_on()
        ),
        Some(Err(err)) => println!("保活：电源请求失败：{err}"),
        None => {}
    }

    let (mut last_power, mut last_locked) = (power, locked);
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let mut changes = 0u32;
    while Instant::now() < deadline {
        let (power, locked) = (display_power(), rc_win32core::screen_is_locked());
        if (power, locked) != (last_power, last_locked) {
            changes += 1;
            println!(
                "{:7} ms：显示器 {last_power:?} → {power:?} · 锁屏 {last_locked} → {locked}",
                started.elapsed().as_millis()
            );
            last_power = power;
            last_locked = locked;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(wake_guard);
    drop(hold_guard);
    println!("期间状态变化 {changes} 次");

    if matches!(power, DisplayPower::Unknown) {
        return Err("没有收到系统的显示器电源通知（熄屏检测不可用）".to_string());
    }
    Ok(())
}

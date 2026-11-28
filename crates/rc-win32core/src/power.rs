//! 电源与桌面状态（FFI 边界模块）。
//!
//! 被控端会话依赖三件事，主界面 / 后台进程 / 诊断命令行共用这一份实现：
//!
//! - [`PowerRequest`]：RAII 电源请求。会话期间阻止系统休眠（`SystemRequired`），
//!   交互会话再追加显示器常亮（`DisplayRequired`）；Drop 时 `PowerClearRequest`。
//! - [`display_is_asleep`] / [`wake_display_if_asleep`]：显示器熄屏状态与唤醒。
//!   显示器熄屏后 DWM 停止合成，DXGI 桌面复制不再出帧，被控端必须先把它点亮；
//!   状态来自系统电源通知 `GUID_CONSOLE_DISPLAY_STATE`，由专用线程接收
//!   （回调经该线程的消息队列派发，所以线程里跑着消息循环）。
//! - [`screen_is_locked`]：当前会话是否处于锁屏（UAC 安全桌面同样算）。
//!   用 WTS 会话标志而不是 `OpenInputDesktop`：部分 Windows 11 版本在锁屏时
//!   仍向后者的调用方返回 `Default` 桌面（本机 26300 实测），会误判成未锁。
//!
//! 安全性：`unsafe` 仅限 windows-rs 投影调用点。通知线程不持有任何窗口或 COM
//! 资源，状态经原子变量发布；`REASON_CONTEXT` 引用的理由字符串随
//! [`PowerRequest`] 存活。

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{GUID, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired, PowerRequestSystemRequired,
    PowerSetRequest, PowerSettingRegisterNotification, SetThreadExecutionState,
    UnregisterPowerSettingNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, ES_DISPLAY_REQUIRED,
    EXECUTION_STATE, HPOWERNOTIFY, POWERBROADCAST_SETTING, POWER_REQUEST_TYPE,
};
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
};
use windows::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE,
};

/// 显示器电源状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPower {
    /// 尚未收到系统通知（或通知线程起不来）。
    Unknown,
    /// 熄屏：DWM 停止合成，桌面复制不出帧。
    Off,
    /// 亮屏。
    On,
    /// 变暗（省电调光，仍在合成、仍能出帧，不算熄屏）。
    Dimmed,
}

impl DisplayPower {
    fn code(self) -> u8 {
        match self {
            DisplayPower::Unknown => 0,
            DisplayPower::Off => 1,
            DisplayPower::On => 2,
            DisplayPower::Dimmed => 3,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => DisplayPower::Off,
            2 => DisplayPower::On,
            3 => DisplayPower::Dimmed,
            _ => DisplayPower::Unknown,
        }
    }
}

/// `GUID_CONSOLE_DISPLAY_STATE`：显示器电源状态通知（数据字节 0 熄屏 / 1 亮屏 / 2 变暗）。
const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID::from_u128(0x6FE69556_704A_47A0_8F24_C28D936FDA47);

/// 唤醒显示器最长等待（与 macOS 参考实现一致）。
const MAX_WAKE_WAIT: Duration = Duration::from_secs(5);
/// 唤醒后每 50 ms 轮询一次是否已亮屏。
const WAKE_POLL_INTERVAL: Duration = Duration::from_millis(50);

static DISPLAY_STATE: AtomicU8 = AtomicU8::new(0);
static STATE_KNOWN: AtomicBool = AtomicBool::new(false);
static STATE_WAITED: AtomicBool = AtomicBool::new(false);
static KNOWN: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static START: OnceLock<()> = OnceLock::new();

/// 发布最新显示器电源状态并唤醒等待首个状态的调用方。
fn publish(state: DisplayPower) {
    DISPLAY_STATE.store(state.code(), Ordering::Release);
    STATE_KNOWN.store(true, Ordering::Release);
    let (lock, signal) = &KNOWN;
    if let Ok(mut known) = lock.lock() {
        *known = true;
        signal.notify_all();
    }
}

/// 电源设置回调：只认显示器电源状态通知。
unsafe extern "system" fn on_power_setting(
    _context: *const c_void,
    kind: u32,
    setting: *const c_void,
) -> u32 {
    if kind == PBT_POWERSETTINGCHANGE {
        let setting = setting as *const POWERBROADCAST_SETTING;
        if !setting.is_null()
            && (*setting).PowerSetting == GUID_CONSOLE_DISPLAY_STATE
            && (*setting).DataLength >= 1
        {
            publish(state_from_notification((*setting).Data[0]));
        }
    }
    ERROR_SUCCESS.0
}

/// 通知线程：注册电源通知 + 消息循环（回调经本线程的消息队列派发），进程存续期间常驻。
fn notify_thread() {
    // 订阅参数放在本线程的栈上：系统只记下回调指针与上下文，栈帧在整个消息循环期间不退出。
    let subscribe = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power_setting),
        Context: std::ptr::null_mut(),
    };
    unsafe {
        let mut registration: *mut c_void = std::ptr::null_mut();
        let status = PowerSettingRegisterNotification(
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(&subscribe as *const _ as *mut c_void),
            &mut registration,
        );
        if status != ERROR_SUCCESS {
            // 通知拿不到：按「状态未知」处理——不算熄屏（宁可多试一次采集，
            // 也不把会话永久挂起）。
            eprintln!(
                "[power] 注册显示器电源通知失败：{}",
                windows::core::Error::new(
                    windows::core::HRESULT::from_win32(status.0),
                    "PowerSettingRegisterNotification",
                )
            );
            publish(DisplayPower::Unknown);
        }
        let mut message = Default::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if !registration.is_null() {
            // HPOWERNOTIFY 在 SDK 里是裸指针，windows-rs 投影成 isize。
            let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(registration as isize));
        }
    }
}

/// 确保通知线程已启动（进程内只起一次）。
fn ensure_tracker() {
    START.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("rc-power-notify".into())
            .spawn(notify_thread);
        if spawned.is_err() {
            publish(DisplayPower::Unknown);
        }
    });
}

/// 当前显示器电源状态。
pub fn display_power() -> DisplayPower {
    ensure_tracker();
    if !STATE_KNOWN.load(Ordering::Acquire) && !STATE_WAITED.swap(true, Ordering::AcqRel) {
        // 注册后系统会立刻推送一次当前状态；只让第一个调用方短暂等它，
        // 免得通知迟迟不来时每次查询都卡一下。
        let deadline = Instant::now() + Duration::from_millis(200);
        let (lock, signal) = &KNOWN;
        let mut known = lock.lock().unwrap_or_else(|error| error.into_inner());
        while !*known {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (updated, _) = signal
                .wait_timeout(known, remaining)
                .unwrap_or_else(|error| error.into_inner());
            known = updated;
        }
    }
    DisplayPower::from_code(DISPLAY_STATE.load(Ordering::Acquire))
}

/// 主显示器是否处于熄屏/休眠状态。
pub fn display_is_asleep() -> bool {
    display_power() == DisplayPower::Off
}

/// 若显示器处于熄屏状态则唤醒亮屏并有界等待生效。
///
/// 唤醒分两步，缺一不可：`SetThreadExecutionState(ES_DISPLAY_REQUIRED)`（一次性）重置
/// 显示器空闲计时、把已熄灭的屏幕点亮——`PowerRequestDisplayRequired` 只能阻止
/// 「到时自动熄屏」，点不亮已经熄了的屏（本机 26300 实测）；随后用电源请求守卫
/// 维持常亮，不再依赖某个线程的执行状态。
///
/// - 已亮屏：零延迟返回 `Ok(None)`；
/// - 已触发唤醒：返回 `Ok(Some(guard))`，调用方在会话结束或不再需要时 Drop 释放，
///   持有期间显示器不会再次自动熄屏；
/// - 等待超时返回 `Err`（守卫一并释放，不影响系统原有熄屏策略）。
pub fn wake_display_if_asleep(
    reason: &str,
    max_wait: Duration,
) -> Result<Option<PowerRequest>, String> {
    if !display_is_asleep() {
        return Ok(None);
    }
    let guard = PowerRequest::create(reason, false, true)?;
    // 返回 0 才是失败（返回上一次的执行状态）。
    if unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) } == EXECUTION_STATE(0) {
        return Err(format!(
            "SetThreadExecutionState 失败：{}",
            windows::core::Error::from_win32()
        ));
    }
    let bounded_wait = max_wait.min(MAX_WAKE_WAIT);
    let start = Instant::now();
    while display_is_asleep() {
        let remaining = bounded_wait.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(format!("显示器唤醒超时（{} ms）", bounded_wait.as_millis()));
        }
        std::thread::sleep(remaining.min(WAKE_POLL_INTERVAL));
    }
    Ok(Some(guard))
}

/// 当前会话是否处于锁屏（UAC 安全桌面同样算锁屏）。
pub fn screen_is_locked() -> bool {
    unsafe {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        let queried = WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        );
        if queried.is_err() || buffer.is_null() {
            return false;
        }
        let locked = bytes as usize >= std::mem::size_of::<WTSINFOEXW>()
            && (*buffer.0.cast::<WTSINFOEXW>())
                .Data
                .WTSInfoExLevel1
                .SessionFlags
                == WTS_SESSIONSTATE_LOCK as i32;
        WTSFreeMemory(buffer.0.cast());
        locked
    }
}

/// 把电源通知里的状态字节翻译成显示器电源状态（纯逻辑，可单测）。
fn state_from_notification(value: u8) -> DisplayPower {
    match value {
        0 => DisplayPower::Off,
        1 => DisplayPower::On,
        2 => DisplayPower::Dimmed,
        _ => DisplayPower::Unknown,
    }
}

/// 电源请求守卫：阻止会话期间系统休眠 / 显示器关闭，Drop 时自动撤销。
///
/// 理由字符串出现在 `powercfg /requests` 输出里；`REASON_CONTEXT` 只记指针，
/// 所以字符串随守卫存活到请求撤销为止。
pub struct PowerRequest {
    handle: HANDLE,
    /// 本守卫生效的请求类型（Drop 时逐项撤销）。
    active: Vec<POWER_REQUEST_TYPE>,
    /// `REASON_CONTEXT` 引用的理由字符串（必须活到请求撤销）。
    _reason: Vec<u16>,
}

// SAFETY: 守卫要跨线程移交（core 把唤醒守卫放进会话级的共享槽位，由会话线程之外的
// 线程 Drop）。内核句柄与线程无关，PowerClearRequest/CloseHandle 在任意线程调用都安全；
// 守卫独占句柄所有权，没有别名访问。
unsafe impl Send for PowerRequest {}

impl PowerRequest {
    /// 开始一个电源请求：阻止系统休眠；`interactive` 时再让显示器保持常亮。
    /// `reason` 出现在 `powercfg /requests` 输出中。
    pub fn begin(reason: &str, interactive: bool) -> Result<Self, String> {
        Self::create(reason, true, interactive)
    }

    fn create(reason: &str, system: bool, display: bool) -> Result<Self, String> {
        let reason_wide: Vec<u16> = reason.encode_utf16().chain(std::iter::once(0)).collect();
        let context = REASON_CONTEXT {
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(reason_wide.as_ptr() as *mut u16),
            },
        };
        let handle = match unsafe { PowerCreateRequest(&context) } {
            Ok(handle) if !handle.is_invalid() => handle,
            Ok(_) => return Err("PowerCreateRequest 失败：返回了无效句柄".to_string()),
            Err(error) => return Err(format!("PowerCreateRequest 失败：{error}")),
        };
        let mut request = Self {
            handle,
            active: Vec::new(),
            _reason: reason_wide,
        };
        for request_type in [
            system.then_some(PowerRequestSystemRequired),
            display.then_some(PowerRequestDisplayRequired),
        ]
        .into_iter()
        .flatten()
        {
            unsafe { PowerSetRequest(request.handle, request_type) }
                .map_err(|error| format!("PowerSetRequest 失败：{error}"))?;
            request.active.push(request_type);
        }
        Ok(request)
    }

    /// 是否包含显示器保持唤醒。
    pub fn keeps_display_on(&self) -> bool {
        self.active.contains(&PowerRequestDisplayRequired)
    }
}

impl Drop for PowerRequest {
    fn drop(&mut self) {
        for request_type in &self.active {
            let _ = unsafe { PowerClearRequest(self.handle, *request_type) };
        }
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_byte_maps_to_display_power() {
        assert_eq!(state_from_notification(0), DisplayPower::Off);
        assert_eq!(state_from_notification(1), DisplayPower::On);
        assert_eq!(state_from_notification(2), DisplayPower::Dimmed);
        assert_eq!(state_from_notification(9), DisplayPower::Unknown);
    }

    #[test]
    fn power_request_is_created_and_revoked_without_error() {
        let request = PowerRequest::begin("RabbitControl unit test", true).expect("创建电源请求");
        assert!(request.keeps_display_on());
        drop(request);
        let request = PowerRequest::begin("RabbitControl unit test", false).expect("创建电源请求");
        assert!(!request.keeps_display_on());
    }

    #[test]
    fn display_and_lock_queries_return_promptly() {
        // 不关心具体值（CI 机器上是哪种状态都有可能），只保证两条查询都不会卡住、不 panic。
        let started = Instant::now();
        let _ = display_is_asleep();
        let _ = screen_is_locked();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

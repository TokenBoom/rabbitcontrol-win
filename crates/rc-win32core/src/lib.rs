//! RabbitControl Windows 系统基础层。
//!
//! 集中管理所有后端共用的 Win32 进程级设施，避免各 crate 各自重复初始化：
//!
//! - **COM 套间** [`ComApartment`]：WASAPI、Media Foundation、WGC、剪贴板
//!   监听都要求在正确初始化的 COM 套间内运行；用 RAII 守卫保证
//!   `CoInitializeEx`/`CoUninitialize` 配对。
//! - **消息窗口** [`MessageWindow`]：Windows 上大量系统事件（剪贴板变更
//!   `WM_CLIPBOARDUPDATE`、托盘回调、显示器拓扑变化 `WM_DISPLAYCHANGE`、
//!   会话锁屏 `WTS_SESSION_LOCK`）只通过窗口消息派发。本类型提供隐藏
//!   消息窗口与专用消息泵线程，各后端以订阅方式接入，而不是各自创建
//!   隐藏窗口。
//! - **线程 QoS** [`promote_current_thread_qos`]：采集/编码/渲染线程经
//!   MMCSS（`AvSetMmThreadCharacteristics`，"Pro Audio"/"Playback" 类别）
//!   注册，获得比普通优先级更稳定的调度延迟。
//! - **电源请求** [`PowerRequest`]：被控会话期间用 `PowerSetRequest`
//!   阻止系统休眠/关屏导致采集中断。
//! - **主机时钟** [`host_time_us`]：基于 `QueryPerformanceCounter` 的
//!   单调微秒时钟，采集帧时间戳与管线时钟的统一基准。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

/// COM 套间模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApartmentModel {
    /// 单线程套间（STA）。WASAPI 事件驱动回调与部分 shell API 要求。
    SingleThreaded,
    /// 多线程套间（MTA）。Media Foundation 与 WGC 推荐。
    MultiThreaded,
}

/// COM 套间初始化守卫，Drop 时调用 `CoUninitialize`。
///
/// TODO: `CoInitializeEx(None, COINIT_APARTMENTTHREADED/COINIT_MULTITHREADED)`；
/// 已初始化（`RPC_E_CHANGED_MODE` 除外）时退化为空守卫。
pub struct ComApartment {
    model: ApartmentModel,
}

impl ComApartment {
    /// 在当前线程初始化 COM 套间。
    pub fn init(model: ApartmentModel) -> std::result::Result<Self, String> {
        // TODO: CoInitializeEx 实现。
        Ok(Self { model })
    }

    /// 套间模型。
    pub fn model(&self) -> ApartmentModel {
        self.model
    }
}

/// 隐藏消息窗口与专用消息泵线程。
///
/// 后端通过订阅消息（剪贴板/显示变更/会话锁定）接入统一消息循环，
/// 避免每个后端各自创建隐藏窗口与线程。
///
/// TODO: 专用线程上 `CreateWindowExW`（`HWND_MESSAGE`）+ `GetMessageW` 泵；
/// 订阅表按消息 ID 分发；Drop 时 `PostMessageW(WM_CLOSE)` 并入线程。
pub struct MessageWindow;

/// 系统消息订阅句柄。Drop 自动退订。
pub struct Subscription;

impl MessageWindow {
    /// 创建消息窗口并启动消息泵线程（进程内幂等单例）。
    pub fn shared() -> std::result::Result<&'static MessageWindow, String> {
        // TODO: OnceLock 单例实现。
        Err("message window not yet implemented".into())
    }

    /// 订阅一类系统消息，回调在消息泵线程执行。
    pub fn subscribe(
        &self,
        _message_id: u32,
        _handler: Box<dyn Fn() + Send>,
    ) -> std::result::Result<Subscription, String> {
        Err("message subscription not yet implemented".into())
    }
}

/// 提升当前线程调度质量（MMCSS）。
///
/// 采集、编码、渲染线程在启动时调用一次即可。
/// TODO: `AvSetMmThreadCharacteristicsW("Pro Audio")`，句柄线程退出前
/// `AvRevertMmThreadCharacteristics`。
pub fn promote_current_thread_qos() {}

/// 电源可用性请求守卫：阻止会话期间系统休眠或显示器关闭。
///
/// TODO: `PowerCreateRequest` + `PowerSetRequest`（`PowerRequestSystemRequired`；
/// 交互会话追加 `PowerRequestDisplayRequired`），Drop 时 `PowerClearRequest`。
pub struct PowerRequest {
    interactive: bool,
}

impl PowerRequest {
    /// 开始一个电源请求。`reason` 出现在 `powercfg /requests` 输出中。
    pub fn begin(_reason: &str, interactive: bool) -> std::result::Result<Self, String> {
        // TODO: PowerCreateRequest/PowerSetRequest 实现。
        Ok(Self { interactive })
    }

    /// 是否包含显示器保持唤醒。
    pub fn keeps_display_on(&self) -> bool {
        self.interactive
    }
}

/// 主机单调时钟（微秒），采集帧时间戳与管线时钟的统一基准。
///
/// TODO: `QueryPerformanceCounter`/`QueryPerformanceFrequency`（启动时缓存
/// 频率倒数避免每次除法）。骨架期以进程内单调时钟代替。
pub fn host_time_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64
}

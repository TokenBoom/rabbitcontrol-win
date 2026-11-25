//! RabbitControl Windows 共享 D3D11 设备上下文。
//!
//! # 为什么独立成 crate
//!
//! Windows 上实现零拷贝视频链路有一个硬约束：同一张纹理要依次经过
//! DXGI Desktop Duplication（采集输出）→ Media Foundation 编码器（输入
//! `IMFDXGIBuffer`）→（远端反向链路）MF 解码器（D3D11VA 输出）→
//! 渲染器（shader resource 上屏），而 D3D11 纹理默认只在**创建它的
//! 设备**上可用。跨设备共享需要 `DXGI_SHARED_RESOURCE` 句柄 + 每步
//! `OpenSharedResource` + 键控互斥（`IDXGIKeyedMutex`）同步，代价和
//! 复杂度都高。
//!
//! 因此全进程统一由本 crate 持有一个 D3D11 设备单例（带多线程保护），
//! 采集/编码/解码/渲染四个后端全部在这个设备上创建纹理，链路各环节
//! 传递裸 `ID3D11Texture2D` 指针即可，无需共享句柄和键控互斥。
//!
//! # 职责
//!
//! - [`SharedDevice`]：进程级 D3D11 设备单例。硬件设备创建失败时回落
//!   WARP 软件设备（远程会话/无 GPU 环境）；TDR（设备移除）时统一重建，
//!   各后端经回调获悉后重建自有资源（与 core 契约 `rebuild` 衔接）。
//! - 纹理辅助：staging 纹理回读 CPU、NV12/BGRA 互转、`resource_guard`
//!   RAII 保活（对齐 core `CapturedFrame::resource_guard` 语义）。
//!
//! # 当前状态
//!
//! 骨架占位：设备创建返回未实现错误。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

/// D3D11 设备类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// 硬件设备（默认）。
    Hardware,
    /// WARP 软件光栅化设备（无 GPU / 远程会话兜底）。
    Warp,
}

/// 进程级共享 D3D11 设备（骨架占位）。
///
/// 实际实现持有一个 `ID3D11Device` + `ID3D11DeviceContext`（多线程
/// 保护经 `ID3D11Multithread`），COM 指针以不透明句柄形式提供给
/// 各后端 crate 还原使用。
pub struct SharedDevice {
    kind: DeviceKind,
}

impl SharedDevice {
    /// 获取或创建进程级设备单例。
    pub fn get() -> std::result::Result<&'static SharedDevice, String> {
        // TODO: OnceLock 幂等创建：D3D11CreateDevice（BGRA 支持必选，
        // 失败回落 WARP）；创建后 SetMultithreadProtected(true)。
        Err("shared d3d11 device not yet implemented".into())
    }

    /// 设备类别。
    pub fn kind(&self) -> DeviceKind {
        self.kind
    }

    /// `ID3D11Device` COM 指针（不透明句柄，调用方不得释放）。
    pub fn device_ptr(&self) -> usize {
        0
    }

    /// `ID3D11DeviceContext` COM 指针（不透明句柄，调用方不得释放；
    /// 访问须先经设备锁同步）。
    pub fn context_ptr(&self) -> usize {
        0
    }

    /// 注册设备移除（TDR）重建回调。回调在新设备就绪后执行一次。
    pub fn on_device_reset(&self, _handler: Box<dyn Fn() + Send>) {
        // TODO: 设备丢失监测线程（CreateEvent + GetDeviceRemovedReason 轮询）。
    }
}

/// 设备移除原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRemovedReason {
    /// 驱动超时检测与恢复（TDR）。
    Hung,
    /// 设备被物理移除/驱动重置。
    Removed,
    /// 驱动升级导致设备重建。
    DriverUpgrade,
    /// 未知原因。
    Unknown,
}

impl SharedDevice {
    #[allow(dead_code)]
    fn placeholder(&self) {}
}

// 骨架期保留字段读取，消除未使用告警。
#[allow(dead_code)]
fn _use_device(d: &SharedDevice) -> DeviceKind {
    d.kind()
}

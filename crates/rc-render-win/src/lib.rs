//! RabbitControl Windows D3D11 渲染器。
//!
//! # 设计
//!
//! - **零拷贝上屏**：解码输出纹理与渲染器同在 [`rc_d3d11::SharedDevice`]
//!   共享设备上，`CreateShaderResourceView` 直接采样；NV12 → RGB 转换
//!   在像素着色器内完成（UV 双平面双采样）。
//! - **呈现目标**： DXGI composition swap chain 绑定到 XAML
//!   `SwapChainPanel`（rc-ui 桌面视口，`ISwapChainPanelNative`），
//!   或独立 `HWND` 交换链（全屏沉浸模式）；硬件设备不可用时共享设备
//!   本身已回落 WARP，渲染路径不变。
//! - **光标合成**：远程光标纹理作为叠加 quad 在同一 pass 渲染，
//!   避免系统光标与远程画面错位。
//!
//! # 当前状态
//!
//! 骨架占位：`present` 返回未实现错误。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use rc_decode::DecodedFrame;
use rc_render::Renderer;
use rc_types::{Error, Result, Timestamp};

/// D3D11 渲染器配置。
#[derive(Debug, Clone, Copy)]
pub struct D3d11RendererConfig {
    /// 渲染目标尺寸（宽, 高）。
    pub size: (u32, u32),
    /// 是否开启垂直同步。
    pub vsync: bool,
    /// 会话起始时刻（延迟统计用）。
    pub session_start: Option<std::time::Instant>,
    /// 是否在 GPU 端合成远程光标。
    pub cursor_compositing: bool,
}

/// D3D11 渲染器（骨架占位）。
pub struct D3d11Renderer {
    cfg: D3d11RendererConfig,
}

impl D3d11Renderer {
    /// 创建独立渲染器（自带交换链）。
    pub fn new(cfg: D3d11RendererConfig) -> Result<Self> {
        // TODO: rc_d3d11::SharedDevice::get() → 编译 NV12 转换着色器 →
        // 创建 composition swap chain。
        Ok(Self { cfg })
    }

    /// 从 XAML `SwapChainPanel` 原生指针创建渲染器。
    ///
    /// `raw` 为 `ISwapChainPanelNative` 接口指针值（由 rc-ui 通过
    /// core 的 `PreallocatedRenderSurface` 传入）。
    pub fn from_raw_panel(_raw: usize, cfg: D3d11RendererConfig) -> Result<Self> {
        // TODO: QueryInterface ISwapChainPanelNative → SetSwapChain。
        Self::new(cfg)
    }

    /// 渲染表面原生指针（交给 core 的 `layer_addr` 挂接路径）。
    pub fn surface_ptr(&self) -> usize {
        0
    }
}

impl Renderer for D3d11Renderer {
    fn present(&mut self, _frame: DecodedFrame) -> Result<Timestamp> {
        // TODO: 采样解码纹理 → 全屏三角形 → Present(0, 0) 非阻塞提交；
        // 返回上屏完成时刻供延迟统计。
        Err(Error::InvalidConfig(
            "d3d11 renderer not yet implemented".into(),
        ))
    }

    fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        // TODO: ResizeBuffers（先释放 render target view）。
        self.cfg.size = size;
        Ok(())
    }

    fn set_vsync(&mut self, on: bool) {
        self.cfg.vsync = on;
    }

    fn set_cursor(&mut self, _pos: (i32, i32), _visible: bool) {
        // TODO: 更新光标 quad 顶点/可见性。
    }
}

//! RabbitControl Windows D3D11 渲染器（对应 macOS `rc-render-macos` / Metal）。
//!
//! # 设计
//!
//! - D3D11 设备 + DXGI composition swap chain，绑定到 XAML `SwapChainPanel`
//!   （rc-ui 桌面视口）或独立交换链；
//! - 解码输出的 [`FrameTexture::D3D11`] 纹理零拷贝作为 shader resource 直接采样，
//!   NV12 → RGB 转换在像素着色器内完成；
//! - 光标合成（对应 macOS `cursor_compositing`）：remote cursor 叠加渲染；
//! - 硬件设备不可用时回落 WARP（与现有 rc-ui `graphics.rs` 一致）。
//!
//! # 当前状态
//!
//! 骨架占位：`present` 返回未实现错误。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use rc_decode::DecodedFrame;
use rc_render::Renderer;
use rc_types::{Error, Result, Timestamp};

/// D3D11 渲染器配置（对应 `MetalRendererConfig`）。
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
        // TODO: D3D11CreateDevice（BGRA 支持 + 失败回落 WARP）。
        Ok(Self { cfg })
    }

    /// 从 XAML `SwapChainPanel` 原生指针创建渲染器（对应 macOS `from_raw_layer`）。
    ///
    /// # Safety
    /// 骨架占位，暂不触碰指针；实现时需要 unsafe 转换 `ISwapChainPanelNative`。
    pub fn from_raw_panel(_raw: usize, cfg: D3d11RendererConfig) -> Result<Self> {
        Self::new(cfg)
    }

    /// 渲染表面原生指针（交给 core 的 `layer_addr` 挂接路径）。
    pub fn surface_ptr(&self) -> usize {
        0
    }
}

impl Renderer for D3d11Renderer {
    fn present(&mut self, _frame: DecodedFrame) -> Result<Timestamp> {
        // TODO: 采样解码纹理 → 全屏三角形 → Present(0, 0)（非阻塞）。
        Err(Error::InvalidConfig(
            "d3d11 renderer not yet implemented".into(),
        ))
    }

    fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        // TODO: ResizeBuffers。
        self.cfg.size = size;
        Ok(())
    }

    fn set_vsync(&mut self, on: bool) {
        self.cfg.vsync = on;
    }

    fn set_cursor(&mut self, _pos: (i32, i32), _visible: bool) {
        // TODO: 光标合成状态。
    }
}

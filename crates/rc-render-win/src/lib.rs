//! RabbitControl Windows D3D11 渲染器（NV12 → RGB，letterbox 呈现）。
//!
//! 解码输出 NV12 纹理 → 双 SRV → BT.709 shader → letterbox 上屏。
//! 共享设备：全部资源创建在 [`rc_d3d11::SharedDevice`] 上。

#![deny(missing_docs)]
#![allow(unsafe_code)] // FFI 边界

use rc_capture::FrameTexture;
use rc_d3d11::SharedDevice;
use rc_decode::DecodedFrame;
use rc_render::Renderer;
use rc_types::{Error, Result, Timestamp};
use windows::core::Interface;
use windows::Win32::Foundation::FALSE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11BlendState, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BLEND_DESC, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_ZERO,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER, D3D11_CULL_NONE, D3D11_FILL_SOLID,
    D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT, D3D11_FLOAT32_MAX, D3D11_RASTERIZER_DESC,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_TEX2D_RTV, D3D11_TEX2D_SRV,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGISwapChain1, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
};

const VS_SRC: &str = r"
struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
VSOut main(uint vid : SV_VertexID) {
    VSOut o;
    float2 xy = float2((vid == 1) ? 1.0 : -1.0, (vid == 2) ? -1.0 : 1.0);
    o.pos = float4(xy, 0.0, 1.0);
    o.uv = float2((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}
";

const PS_NV12_SRC: &str = r"
Texture2D texY  : register(t0);
Texture2D texUV : register(t1);
SamplerState samp : register(s0);
float4 main(float2 uv : TEXCOORD0) : SV_Target {
    float y  = texY.Sample(samp, uv).r;
    float2 c = texUV.Sample(samp, uv).rg - 0.5;
    float3 rgb;
    rgb.r = y + 1.5748 * c.x;
    rgb.g = y - 0.1873 * c.x - 0.4681 * c.y;
    rgb.b = y + 1.8556 * c.y;
    return float4(saturate(rgb), 1.0);
}
";

/// Windows D3D11 渲染器。
pub struct RendererWin {
    swapchain: IDXGISwapChain1,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    rtv: ID3D11RenderTargetView,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    vsync: bool,
    video_size: (u32, u32),
    swap_size: (u32, u32),
    #[allow(dead_code)] // 保活上一帧 SRV，防 GPU 引用悬垂
    prev_srvs: Option<[ID3D11ShaderResourceView; 2]>,
}

unsafe impl Send for RendererWin {}

impl RendererWin {
    /// 创建渲染器。
    ///
    /// - `swapchain_raw`：UI 预分配的 `IDXGISwapChain1` COM 指针；
    ///   `None` 时自建 composition 交换链（诊断/测试路径）。
    pub fn new(swapchain_raw: Option<usize>) -> Result<Self> {
        let shared = SharedDevice::get().map_err(Error::InvalidConfig)?;
        let device = shared.device();
        let context = shared.context();

        let swapchain = match swapchain_raw {
            Some(ptr) => {
                let raw = ptr as *mut core::ffi::c_void;
                unsafe { IDXGISwapChain1::from_raw_borrowed(&raw) }
                    .ok_or_else(|| Error::InvalidConfig("交换链指针无效".into()))?
                    .clone()
            }
            None => create_composition_swapchain(&device, 640, 360)?,
        };

        let swap_size = swapchain_size_of(&swapchain);
        let rtv = create_backbuffer_rtv(&device, &swapchain)?;
        let vs = compile_vs(&device)?;
        let ps = compile_ps(&device, PS_NV12_SRC)?;
        let sampler = create_sampler(&device)?;

        let r = Self {
            swapchain,
            device: device.clone(),
            context,
            rtv,
            vs,
            ps,
            sampler,
            vsync: false,
            video_size: (0, 0),
            swap_size,
            prev_srvs: None,
        };
        r.setup_states();
        Ok(r)
    }

    fn setup_states(&self) {
        let blend = D3D11_BLEND_DESC {
            AlphaToCoverageEnable: FALSE,
            IndependentBlendEnable: FALSE,
            RenderTarget: [D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: FALSE,
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_ZERO,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_ZERO,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            }; 8],
        };
        let mut blend_state: Option<ID3D11BlendState> = None;
        let _ = unsafe { self.device.CreateBlendState(&blend, Some(&mut blend_state)) };
        let raster = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            FrontCounterClockwise: FALSE,
            DepthBias: 0,
            DepthBiasClamp: 0.0,
            SlopeScaledDepthBias: 0.0,
            DepthClipEnable: FALSE,
            ScissorEnable: FALSE,
            MultisampleEnable: FALSE,
            AntialiasedLineEnable: FALSE,
        };
        let mut raster_state: Option<ID3D11RasterizerState> = None;
        let _ = unsafe {
            self.device
                .CreateRasterizerState(&raster, Some(&mut raster_state))
        };
        unsafe {
            if let Some(b) = blend_state.as_ref() {
                self.context
                    .OMSetBlendState(Some(b), Some(&[0.0; 4]), 0xFFFF_FFFF);
            }
            if let Some(r) = raster_state.as_ref() {
                self.context.RSSetState(r);
            }
        }
    }

    fn viewport(&self) -> D3D11_VIEWPORT {
        let (sw, sh) = self.swap_size;
        let (vw, vh) = if self.video_size.0 == 0 {
            (sw, sh)
        } else {
            self.video_size
        };
        if sw == 0 || sh == 0 || vw == 0 || vh == 0 {
            return D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: 0.0,
                Height: 0.0,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
        }
        let scale = (sw as f32 / vw as f32).min(sh as f32 / vh as f32);
        D3D11_VIEWPORT {
            TopLeftX: (sw as f32 - vw as f32 * scale) * 0.5,
            TopLeftY: (sh as f32 - vh as f32 * scale) * 0.5,
            Width: vw as f32 * scale,
            Height: vh as f32 * scale,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }
    }

    fn nv12_srvs(
        device: &ID3D11Device,
        tex: &ID3D11Texture2D,
    ) -> windows::core::Result<[ID3D11ShaderResourceView; 2]> {
        let mk = |format| D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut y = None;
        unsafe {
            device.CreateShaderResourceView(tex, Some(&mk(DXGI_FORMAT_R8_UNORM)), Some(&mut y))
        }?;
        let mut uv = None;
        unsafe {
            device.CreateShaderResourceView(tex, Some(&mk(DXGI_FORMAT_R8G8_UNORM)), Some(&mut uv))
        }?;
        Ok([
            y.ok_or_else(windows::core::Error::from_win32)?,
            uv.ok_or_else(windows::core::Error::from_win32)?,
        ])
    }
}

fn mfw(e: windows::core::Error) -> Error {
    Error::InvalidConfig(format!("{e:?}"))
}

fn swapchain_size_of(chain: &IDXGISwapChain1) -> (u32, u32) {
    let desc = unsafe { chain.GetDesc() }.unwrap_or_default();
    (desc.BufferDesc.Width, desc.BufferDesc.Height)
}

fn create_backbuffer_rtv(
    device: &ID3D11Device,
    chain: &IDXGISwapChain1,
) -> Result<ID3D11RenderTargetView> {
    let back: ID3D11Texture2D = unsafe { chain.GetBuffer(0) }.map_err(mfw)?;
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
        },
    };
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(&back, Some(&desc), Some(&mut rtv)) }.map_err(mfw)?;
    rtv.ok_or_else(|| Error::InvalidConfig("CreateRenderTargetView 未返回".into()))
}

fn create_composition_swapchain(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device.cast().map_err(mfw)?;
    let adapter = unsafe { dxgi.GetAdapter() }.map_err(mfw)?;
    let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory2 =
        unsafe { adapter.GetParent() }.map_err(mfw)?;
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: FALSE,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: windows::Win32::Graphics::Dxgi::DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_NONE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: windows::Win32::Graphics::Dxgi::Common::DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    unsafe { factory.CreateSwapChainForComposition(device, &desc, None) }.map_err(mfw)
}

fn compile_blob(src: &str, entry: &str, target: &str) -> windows::core::Result<ID3DBlob> {
    let entry_z = format!("{entry}\0");
    let target_z = format!("{target}\0");
    let mut blob = None;
    let mut err = None;
    unsafe {
        D3DCompile(
            src.as_ptr() as *const core::ffi::c_void,
            src.len(),
            None,
            None,
            None,
            windows::core::PCSTR::from_raw(entry_z.as_ptr()),
            windows::core::PCSTR::from_raw(target_z.as_ptr()),
            0,
            0,
            &mut blob,
            Some(&mut err),
        )
    }?;
    blob.ok_or_else(windows::core::Error::from_win32)
}

fn compile_vs(device: &ID3D11Device) -> Result<ID3D11VertexShader> {
    let blob = compile_blob(VS_SRC, "main", "vs_5_0").map_err(mfw)?;
    let mut vs = None;
    unsafe {
        device.CreateVertexShader(
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()),
            None,
            Some(&mut vs),
        )
    }
    .map_err(mfw)?;
    vs.ok_or_else(|| Error::InvalidConfig("CreateVertexShader 未返回".into()))
}

fn compile_ps(device: &ID3D11Device, src: &str) -> Result<ID3D11PixelShader> {
    let blob = compile_blob(src, "main", "ps_5_0").map_err(mfw)?;
    let mut ps = None;
    unsafe {
        device.CreatePixelShader(
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()),
            None,
            Some(&mut ps),
        )
    }
    .map_err(mfw)?;
    ps.ok_or_else(|| Error::InvalidConfig("CreatePixelShader 未返回".into()))
}

fn create_sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: D3D11_FLOAT32_MAX,
    };
    let mut state = None;
    unsafe { device.CreateSamplerState(&desc, Some(&mut state)) }.map_err(mfw)?;
    state.ok_or_else(|| Error::InvalidConfig("CreateSamplerState 未返回".into()))
}

impl Renderer for RendererWin {
    fn present(&mut self, frame: DecodedFrame) -> Result<Timestamp> {
        let FrameTexture::D3D11(raw) = frame.texture else {
            return Err(Error::InvalidFrame(format!(
                "render-win 仅支持 D3D11 纹理，收到 {:?}",
                frame.texture
            )));
        };
        // 所有权接收：解码器移交的 1 次 COM 引用，呈现后释放。
        let texture: ID3D11Texture2D =
            unsafe { std::mem::transmute::<*mut core::ffi::c_void, ID3D11Texture2D>(raw) };

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        self.video_size = (desc.Width, desc.Height);

        let srvs = Self::nv12_srvs(&self.device, &texture).map_err(mfw)?;
        let viewport = self.viewport();
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(self.rtv.clone())]), None);
            self.context
                .ClearRenderTargetView(&self.rtv, &[0.0f32, 0.0, 0.0, 1.0]);
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context
                .PSSetShaderResources(0, Some(&[Some(srvs[0].clone()), Some(srvs[1].clone())]));
            self.context.Draw(4, 0);
        }
        let interval = if self.vsync { 1u32 } else { 0u32 };
        let present_result = unsafe {
            self.swapchain
                .Present(interval, windows::Win32::Graphics::Dxgi::DXGI_PRESENT(0))
        };
        drop(srvs);
        drop(texture);
        if present_result.is_err() {
            self.rtv = create_backbuffer_rtv(&self.device, &self.swapchain)?;
            let hr_err = windows::core::Error::from_hresult(present_result);
            return Err(Error::InvalidFrame(format!("Present 失败：{hr_err}")));
        }
        Ok(Timestamp(rc_win32core::host_time_us() as u32))
    }

    fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        if size.0 == 0 || size.1 == 0 {
            return Err(Error::InvalidConfig("resize 尺寸非法".into()));
        }
        // 先释放 RTV 引用（ResizeBuffers 要求所有绑定为空）
        // 先释放 RTV 引用（ResizeBuffers 要求所有绑定为空）
        let empty_rtv: Option<ID3D11RenderTargetView> = None;
        self.rtv = unsafe {
            core::mem::transmute::<Option<ID3D11RenderTargetView>, ID3D11RenderTargetView>(
                empty_rtv,
            )
        };
        unsafe {
            self.swapchain.ResizeBuffers(
                0,
                size.0,
                size.1,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .map_err(mfw)?;
        self.swap_size = size;
        self.rtv = create_backbuffer_rtv(&self.device, &self.swapchain)?;
        Ok(())
    }

    fn set_vsync(&mut self, on: bool) {
        self.vsync = on;
    }

    fn set_cursor(&mut self, _pos: (i32, i32), _visible: bool) {
        // D7 光标本地叠加：M3 实现。
    }
}

/// platform-win 兼容配置。
#[derive(Debug, Clone)]
pub struct D3d11RendererConfig {
    /// 尺寸 (宽, 高)。
    pub size: (u32, u32),
    /// 垂直同步。
    pub vsync: bool,
    /// 会话起始时间（未使用，保留兼容）。
    pub session_start: Option<std::time::Instant>,
    /// 光标合成（未使用，保留兼容）。
    pub cursor_compositing: bool,
}

/// platform-win 兼容渲染器别名。
pub struct D3d11Renderer {
    inner: RendererWin,
}

impl D3d11Renderer {
    /// 创建渲染器。
    pub fn new(_cfg: D3d11RendererConfig) -> Result<Self> {
        Ok(Self {
            inner: RendererWin::new(Some(0))?,
        })
    }

    /// 从预分配表面创建（领养交换链）。
    pub fn from_raw_panel(raw: usize, _cfg: D3d11RendererConfig) -> Result<Self> {
        Ok(Self {
            inner: RendererWin::new(Some(raw))?,
        })
    }

    /// 表面指针（诊断用）。
    pub fn surface_ptr(&self) -> usize {
        0
    }

    /// 渲染器引用。
    pub fn renderer(&self) -> &RendererWin {
        &self.inner
    }

    /// 渲染器可变引用。
    pub fn renderer_mut(&mut self) -> &mut RendererWin {
        &mut self.inner
    }
}

impl Renderer for D3d11Renderer {
    fn present(&mut self, frame: DecodedFrame) -> Result<Timestamp> {
        self.inner.present(frame)
    }
    fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        self.inner.resize(size)
    }
    fn set_vsync(&mut self, on: bool) {
        self.inner.set_vsync(on)
    }
    fn set_cursor(&mut self, pos: (i32, i32), visible: bool) {
        self.inner.set_cursor(pos, visible)
    }
}

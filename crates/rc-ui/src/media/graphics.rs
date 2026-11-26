use std::ptr;

use rc_client_core::platform::PreallocatedRenderSurface;
use windows::{
    core::{Interface as Interface37, Result},
    Win32::{
        Foundation::E_UNEXPECTED,
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
                D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
            },
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        System::WinRT::Xaml::ISwapChainPanelNative,
    },
    UI::Xaml::Controls::SwapChainPanel,
};

pub struct Surface {
    panel: ISwapChainPanelNative,
    chain: IDXGISwapChain1,
    context: ID3D11DeviceContext,
    target: ID3D11RenderTargetView,
}

impl Surface {
    pub fn new(panel: &SwapChainPanel, width: u32, height: u32) -> Result<Self> {
        let (device, context) = create_device(D3D_DRIVER_TYPE_HARDWARE)
            .or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))?;
        let panel: ISwapChainPanelNative = panel.cast()?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        unsafe {
            let dxgi: IDXGIDevice = device.cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let chain = factory.CreateSwapChainForComposition(&device, &desc, None)?;
            let buffer: ID3D11Texture2D = chain.GetBuffer(0)?;
            let target = device.CreateRenderTargetView(&buffer, ptr::null())?;
            panel.SetSwapChain(&chain)?;
            Ok(Self {
                panel,
                chain,
                context,
                target,
            })
        }
    }

    pub fn present_idle(&self) -> Result<()> {
        let color = [0.095, 0.16, 0.20, 1.0];
        unsafe {
            self.context
                .ClearRenderTargetView(&self.target, color.as_ptr());
            self.chain.Present(1, 0)
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            let _ = self.panel.SetSwapChain(None);
        }
    }
}

fn create_device(driver: D3D_DRIVER_TYPE) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            &[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0],
            D3D11_SDK_VERSION,
            &mut device,
            ptr::null_mut(),
            &mut context,
        )?;
    }
    Ok((device.ok_or(E_UNEXPECTED)?, context.ok_or(E_UNEXPECTED)?))
}

/// 会话渲染表面守卫：保活 0.58 世界交换链 COM 引用直至会话结束。
/// 面板绑定（SetSwapChain）内部已 AddRef，不需要跨线程解绑（WinRT UI 对象
/// 不宜跨线程调用，下一会话重建时直接覆写绑定）。
struct SessionSurfaceGuard {
    _chain: windows58::Win32::Graphics::Dxgi::IDXGISwapChain1,
}
// DXGI 对象自由线程；守卫随会话值一次性移交会话线程，创建线程不再访问
unsafe impl Send for SessionSurfaceGuard {}
unsafe impl Sync for SessionSurfaceGuard {}

/// 会话渲染表面：**共享设备**上的 composition 交换链（解码输出纹理同设备可画，
/// 零拷贝前提），绑定 `SwapChainPanel` 后以裸指针 + 保活守卫交会话线程渲染器领养。
///
/// 初始 1280×720；后续尺寸由面板 `SizeChanged` → `StageRect` → `Renderer::resize` 联动。
/// WinRT 面板投影仅在 0.37 存在：跨 world 以裸指针借用绑定（COM ABI 同构）。
pub fn session_render_surface(
    panel: &SwapChainPanel,
) -> std::result::Result<PreallocatedRenderSurface, String> {
    use std::mem::ManuallyDrop;
    use windows::Win32::Graphics::Dxgi::IDXGISwapChain as SwapChain37;
    use windows::Win32::System::WinRT::Xaml::ISwapChainPanelNative as PanelNative37;

    let shared = rc_d3d11::SharedDevice::get()?;
    let device = shared.device();
    let native: PanelNative37 = panel.cast().map_err(|e| e.to_string())?;
    let desc = windows58::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_DESC1 {
        Width: 1280,
        Height: 720,
        Format: windows58::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows58::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: windows58::Win32::Graphics::Dxgi::DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: windows58::Win32::Graphics::Dxgi::DXGI_SCALING_STRETCH,
        SwapEffect: windows58::Win32::Graphics::Dxgi::DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: windows58::Win32::Graphics::Dxgi::Common::DXGI_ALPHA_MODE_PREMULTIPLIED,
        ..Default::default()
    };
    unsafe {
        let dxgi: windows58::Win32::Graphics::Dxgi::IDXGIDevice =
            windows58::core::Interface::cast(&device).map_err(|e| e.to_string())?;
        let factory: windows58::Win32::Graphics::Dxgi::IDXGIFactory2 = dxgi
            .GetAdapter()
            .and_then(|a| a.GetParent())
            .map_err(|e| e.to_string())?;
        let chain = factory
            .CreateSwapChainForComposition(&device, &desc, None)
            .map_err(|e| e.to_string())?;
        // 跨 world：0.58 交换链裸指针 → 0.37 基础接口零所有权借用（不 Release）
        let raw = windows58::core::Interface::as_raw(&chain) as usize;
        let borrowed: ManuallyDrop<SwapChain37> = std::mem::transmute(raw);
        native.SetSwapChain(&*borrowed).map_err(|e| e.to_string())?;
        Ok(PreallocatedRenderSurface::new(
            raw,
            SessionSurfaceGuard { _chain: chain },
        ))
    }
}

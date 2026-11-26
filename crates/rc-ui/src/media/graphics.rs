use std::ptr;
use windows::{
    core::{Interface, Result},
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

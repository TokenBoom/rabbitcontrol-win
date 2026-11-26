//! RabbitControl Windows 共享 D3D11 设备上下文。
//!
//! 全进程统一由本 crate 持有一个 D3D11 设备单例（带多线程保护），
//! 采集/编码/解码/渲染四个后端全部在这个设备上创建纹理，链路各环节
//! 传递裸 `ID3D11Texture2D` 指针即可，无需共享句柄和键控互斥。
//!
//! # 当前状态
//!
//! [`SharedDevice`] 单例 + NV12 纹理创建已实现；TDR 重建联动在 M3 完善。

#![deny(missing_docs)]
#![allow(unsafe_code)] // 本 crate 整体为 FFI 边界：unsafe 仅限 windows-rs 投影调用点

pub mod probe;

use std::sync::{Arc, Mutex, OnceLock};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

/// D3D11 设备类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// 硬件设备（默认）。
    Hardware,
    /// WARP 软件光栅化设备（无 GPU / 远程会话兜底）。
    Warp,
}

struct SharedDeviceInner {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

/// 进程级共享 D3D11 设备单例。
pub struct SharedDevice {
    kind: DeviceKind,
    inner: Arc<Mutex<SharedDeviceInner>>,
}

static SHARED_DEVICE: OnceLock<std::result::Result<Arc<SharedDevice>, String>> = OnceLock::new();

impl SharedDevice {
    /// 获取或创建进程级设备单例。硬件设备创建失败时回落 WARP。
    pub fn get() -> std::result::Result<Arc<SharedDevice>, String> {
        if let Some(result) = SHARED_DEVICE.get() {
            return result.as_ref().map(Arc::clone).map_err(|e| e.clone());
        }
        let created = Self::create().map(Arc::new);
        match SHARED_DEVICE.set(created.clone()) {
            Ok(()) => created.as_ref().map(Arc::clone).map_err(|e| e.clone()),
            Err(_) => SHARED_DEVICE
                .get()
                .expect("OnceLock 已被并发初始化")
                .as_ref()
                .map(Arc::clone)
                .map_err(|e| e.clone()),
        }
    }

    fn create() -> std::result::Result<SharedDevice, String> {
        let attempts: [(DeviceKind, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE); 2] = [
            (DeviceKind::Hardware, D3D_DRIVER_TYPE_HARDWARE),
            (DeviceKind::Warp, D3D_DRIVER_TYPE_WARP),
        ];
        let mut last_err = String::from("未执行任何创建尝试");
        for (kind, driver_type) in attempts {
            match Self::try_create_driver(driver_type) {
                Ok((device, context)) => {
                    let mt: ID3D11Multithread = device.cast().map_err(|e| e.to_string())?;
                    unsafe {
                        let _ = mt.SetMultithreadProtected(true);
                    }
                    return Ok(Self {
                        kind,
                        inner: Arc::new(Mutex::new(SharedDeviceInner { device, context })),
                    });
                }
                Err(err) => last_err = format!("{kind:?} 设备创建失败：{err}"),
            }
        }
        Err(last_err)
    }

    fn try_create_driver(
        driver_type: windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE,
    ) -> std::result::Result<(ID3D11Device, ID3D11DeviceContext), String> {
        let mut device = None;
        let mut context = None;
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                driver_type,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[
                    windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0,
                    windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_0,
                ]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        hr.map_err(|err| format!("{err:?}"))?;
        let device = device.ok_or_else(|| "D3D11CreateDevice 未返回设备".to_string())?;
        let context = context.ok_or_else(|| "D3D11CreateDevice 未返回上下文".to_string())?;
        Ok((device, context))
    }

    /// 设备类别。
    pub fn kind(&self) -> DeviceKind {
        self.kind
    }

    /// 设备 COM 引用（Clone，借用安全）。
    pub fn device(&self) -> ID3D11Device {
        self.inner.lock().expect("shared device lock").device.clone()
    }

    /// 上下文 COM 引用（Clone，借用安全）。
    pub fn context(&self) -> ID3D11DeviceContext {
        self.inner.lock().expect("shared device lock").context.clone()
    }

    /// 在共享设备上创建一张 NV12 纹理（解码输出落地 / 编码输入载体）。
    pub fn create_nv12_texture(
        &self,
        width: u32,
        height: u32,
    ) -> std::result::Result<ID3D11Texture2D, String> {
        let inner = self.inner.lock().expect("shared device lock");
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe {
            inner.device.CreateTexture2D(&desc, None, Some(&mut texture))
        }
        .map_err(|err| format!("创建 NV12 纹理失败：{err:?}"))?;
        texture.ok_or_else(|| "CreateTexture2D 未返回纹理".to_string())
    }


}

/// 诊断：设备移除原因（占位枚举，M3 完善）。
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

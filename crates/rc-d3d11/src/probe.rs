//! D3D11 设备能力探测（诊断入口，供 `rc-cli probe` 与后续能力上报）。
//!
//! 安全性：`unsafe` 仅限 windows-rs 投影调用点，输出参数均为栈变量。

#![allow(unsafe_code)]

use crate::DeviceKind;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};

/// D3D11 设备探测结果。
#[derive(Debug, Clone)]
pub struct DeviceProbe {
    /// 设备类别（硬件 / WARP 兜底）。
    pub kind: DeviceKind,
    /// 适配器描述。
    pub adapter_description: String,
    /// 厂商 PCI ID。
    pub vendor_id: u32,
    /// 设备 PCI ID。
    pub device_id: u32,
    /// 专用显存（字节）。
    pub dedicated_video_memory_bytes: u64,
    /// 生效特性级别原始值（如 0xB000 = FL 11.0）。
    pub feature_level: u32,
}

/// 探测设备创建能力：优先硬件设备，失败回落 WARP；两者皆失败上抛末次错误。
pub fn probe_device() -> std::result::Result<DeviceProbe, String> {
    let attempts = [
        (DeviceKind::Hardware, D3D_DRIVER_TYPE_HARDWARE),
        (DeviceKind::Warp, D3D_DRIVER_TYPE_WARP),
    ];
    let mut last_err = String::from("未执行任何创建尝试");
    for (kind, driver_type) in attempts {
        match try_create(driver_type) {
            Ok((device, feature_level)) => return describe(kind, &device, feature_level),
            Err(err) => last_err = format!("{kind:?} 设备创建失败：{err}"),
        }
    }
    Err(last_err)
}

fn try_create(
    driver_type: D3D_DRIVER_TYPE,
) -> std::result::Result<(ID3D11Device, D3D_FEATURE_LEVEL), String> {
    let mut device = None;
    let mut context = None;
    let mut feature_level = D3D_FEATURE_LEVEL(0);
    let hr = unsafe {
        D3D11CreateDevice(
            None,
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
    };
    hr.map_err(|err| format!("{err:?}"))?;
    let device = device.ok_or_else(|| "D3D11CreateDevice 未返回设备".to_string())?;
    Ok((device, feature_level))
}

fn describe(
    kind: DeviceKind,
    device: &ID3D11Device,
    feature_level: D3D_FEATURE_LEVEL,
) -> std::result::Result<DeviceProbe, String> {
    let dxgi: IDXGIDevice = device
        .cast()
        .map_err(|err| format!("设备未暴露 IDXGIDevice：{err:?}"))?;
    let adapter =
        unsafe { dxgi.GetAdapter() }.map_err(|err| format!("GetAdapter 失败：{err:?}"))?;
    let adapter: IDXGIAdapter1 = adapter
        .cast()
        .map_err(|err| format!("适配器未暴露 IDXGIAdapter1：{err:?}"))?;
    let desc = unsafe { adapter.GetDesc1() }.map_err(|err| format!("GetDesc1 失败：{err:?}"))?;
    Ok(DeviceProbe {
        kind,
        adapter_description: rc_win32core::wide_to_string(&desc.Description),
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        dedicated_video_memory_bytes: desc.DedicatedVideoMemory as u64,
        feature_level: feature_level.0 as u32,
    })
}

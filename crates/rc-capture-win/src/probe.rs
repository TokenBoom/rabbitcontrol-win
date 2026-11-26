//! DXGI 适配器/输出枚举探测（诊断入口）。
//!
//! 安全性：`unsafe` 仅限 windows-rs 投影调用点，输出参数均为栈变量。

#![allow(unsafe_code)]

use rc_win32core::wide_to_string;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ERROR_NOT_FOUND};

/// 单个显示输出探测结果。
#[derive(Debug, Clone)]
pub struct OutputProbe {
    /// 所属适配器描述。
    pub adapter_description: String,
    /// 适配器厂商 PCI ID。
    pub vendor_id: u32,
    /// 输出设备名（如 `\\.\DISPLAY1`）。
    pub output_name: String,
    /// 桌面坐标（左、上、右、下）。
    pub desktop_rect: (i32, i32, i32, i32),
    /// 旋转（DXGI_MODE_ROTATION 原始值，0/1 = 无旋转）。
    pub rotation: u32,
    /// 是否已接入桌面。
    pub attached_to_desktop: bool,
}

/// 枚举全部适配器与其显示输出（诊断视图）。
pub fn probe_outputs() -> std::result::Result<Vec<OutputProbe>, String> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|err| format!("CreateDXGIFactory1 失败：{err:?}"))?;
    let mut outputs = Vec::new();
    let mut adapter_index = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(err) => {
                return Err(format!("EnumAdapters1({adapter_index}) 失败：{err:?}"));
            }
        };
        adapter_index += 1;
        let adapter_desc =
            unsafe { adapter.GetDesc1() }.map_err(|err| format!("GetDesc1 失败：{err:?}"))?;
        let adapter_description = wide_to_string(&adapter_desc.Description);
        let mut output_index = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(err) => {
                    return Err(format!("EnumOutputs({output_index}) 失败：{err:?}"));
                }
            };
            output_index += 1;
            let desc =
                unsafe { output.GetDesc() }.map_err(|err| format!("输出 GetDesc 失败：{err:?}"))?;
            let rect = &desc.DesktopCoordinates;
            outputs.push(OutputProbe {
                adapter_description: adapter_description.clone(),
                vendor_id: adapter_desc.VendorId,
                output_name: wide_to_string(&desc.DeviceName),
                desktop_rect: (rect.left, rect.top, rect.right, rect.bottom),
                rotation: desc.Rotation.0 as u32,
                attached_to_desktop: desc.AttachedToDesktop.as_bool(),
            });
        }
    }
    Ok(outputs)
}

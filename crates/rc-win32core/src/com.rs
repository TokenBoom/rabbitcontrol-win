//! COM 套间初始化（FFI 边界模块）。
//!
//! 安全性：`unsafe` 仅限 windows-rs 投影调用点，输出参数均为栈变量，
//! 不自造指针或跨线程转移 COM 生命周期。

#![allow(unsafe_code)]

use windows::Win32::{
    Foundation::RPC_E_CHANGED_MODE,
    System::Com::{
        CoInitializeEx, CoUninitialize, COINIT, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
    },
};

/// COM 套间模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApartmentModel {
    /// 单线程套间（STA）。WASAPI 事件驱动回调与部分 shell API 要求。
    SingleThreaded,
    /// 多线程套间（MTA）。Media Foundation 与 WGC 推荐。
    MultiThreaded,
}

/// COM 套间初始化守卫，Drop 时按所有权调用 `CoUninitialize`。
pub struct ComApartment {
    model: ApartmentModel,
    owns_init: bool,
}

impl ComApartment {
    /// 在当前线程初始化 COM 套间。
    ///
    /// 线程已处于其它套间模型时（`RPC_E_CHANGED_MODE`）返回不拥有
    /// 初始化所有权的守卫——套间已就绪，由先前的持有者负责其生命周期；
    /// 其余失败原样上抛。
    pub fn init(model: ApartmentModel) -> std::result::Result<Self, String> {
        let coinit: COINIT = match model {
            ApartmentModel::SingleThreaded => COINIT_APARTMENTTHREADED,
            ApartmentModel::MultiThreaded => COINIT_MULTITHREADED,
        };
        let hr = unsafe { CoInitializeEx(None, coinit) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(format!("CoInitializeEx 失败：{hr:?}"));
        }
        Ok(Self {
            model,
            owns_init: hr.is_ok(),
        })
    }

    /// 套间模型。
    pub fn model(&self) -> ApartmentModel {
        self.model
    }

    /// 当前守卫是否拥有本次 `CoInitializeEx` 的生命周期。
    pub fn owns_init(&self) -> bool {
        self.owns_init
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owns_init {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

//! RabbitControl Windows 音频后端（WASAPI）。
//!
//! # 采集（被控侧）
//!
//! 默认渲染端点的环回采集：`IMMDeviceEnumerator` 取 eRender/eConsole
//! 默认设备，`AUDCLNT_STREAMFLAGS_LOOPBACK` + 事件驱动共享模式
//! 初始化 `IAudioClient`，专用线程泵 `IAudioCaptureClient` 数据包；
//! 系统混音格式（float32 交错）就地转换为 core 要求的 48 kHz /
//! 16-bit PCM（必要时先经 `IAudioResampler` MFT 重采样）。
//!
//! # 播放（主控侧）
//!
//! 共享模式低延迟播放：事件驱动 + 最小缓冲周期，push 模型对齐
//! core 的 `AudioPlayer`。
//!
//! COM 套间初始化统一走 [`rc_win32core::ComApartment`]。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::sync::Arc;

use rc_audio::{AudioBackendProvider, AudioCapture, AudioPlayer, PcmFrame};

/// WASAPI 环回采集（骨架占位）。
pub struct WasapiLoopbackCapture {
    sample_rate: u32,
    channels: u8,
}

impl AudioCapture for WasapiLoopbackCapture {
    fn try_recv_frame(&mut self) -> Option<PcmFrame> {
        // TODO: IAudioCaptureClient::GetNextPacketSize / GetBuffer 泵送。
        None
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn stop(&mut self) {}
}

/// WASAPI 播放器（骨架占位）。
pub struct WasapiPlayer {
    muted: bool,
}

impl AudioPlayer for WasapiPlayer {
    fn push_frame(&mut self, _frame: &PcmFrame) -> std::result::Result<(), String> {
        Err("wasapi playback not yet implemented".into())
    }

    fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    fn is_muted(&self) -> bool {
        self.muted
    }

    fn stop(&mut self) {}
}

struct WasapiProvider;

impl AudioBackendProvider for WasapiProvider {
    fn create_system_audio_capture(&self) -> std::result::Result<Box<dyn AudioCapture>, String> {
        // TODO: IMMDeviceEnumerator → 默认渲染端点 → loopback 初始化。
        Err("wasapi loopback capture not yet implemented".into())
    }

    fn create_audio_player(
        &self,
        _sample_rate: u32,
        _channels: u8,
    ) -> std::result::Result<Box<dyn AudioPlayer>, String> {
        Err("wasapi playback not yet implemented".into())
    }
}

/// 注册 WASAPI 音频提供者（幂等）。
pub fn register() -> bool {
    rc_audio::register_audio_provider(Arc::new(WasapiProvider))
}

//! RabbitControl Windows 硬件视频编码器（Media Foundation）。
//!
//! - `MFTEnumEx` 选硬件异步编码 MFT（H.264，输入 NV12）；
//! - `IMFDXGIDeviceManager` 绑共享设备——采集交来的 NV12 纹理零拷贝入；
//! - 异步事件驱动（`METransformNeedInput` / `METransformHaveOutput`），
//!   CBR + 低延迟 + 无限 GOP（IDR 按需请求）；
//! - 实装见 [`encoder::MfEncoder`]（M5a.3）。

pub mod encoder;
pub mod probe;

pub use encoder::MfEncoder;

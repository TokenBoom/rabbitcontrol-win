//! `hw-loop` 子命令：合成帧 → MF 编码 → MF 解码 → 读回校验，单独验证编解码链路。
//!
//! 不经过采集，所以锁屏 / 熄屏时也能跑。被控端「有画面但对方看不到」时用它区分
//! 「编解码器本身坏了」与「采集没出帧 / 传输有问题」：
//!
//! ```text
//! rc-cli hw-loop --width 1920 --height 1080 --fps 30 --seconds 3
//! ```
//!
//! 合成帧是一张张只有亮度变化的灰阶图（16 → 235 扫描，色度中性），每帧编完立刻解码，
//! 再把中心像素读回来和源值比对。

use std::time::{Duration, Instant};

use rc_decode::{DecodeOutput, Decoder};
use rc_encode::{EncodedFrame, Encoder, EncoderConfig};
use rc_types::{FrameKind, Resolution, Timestamp};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

/// 解码后亮度与合成值的允许误差（有损编码 + 有限范围量化）。
const LUMA_TOLERANCE: i32 = 8;
/// 中性色度（U = V = 128）的允许误差。
const CHROMA_TOLERANCE: i32 = 6;
/// 单帧编 / 解码的超时预算。
const FRAME_DEADLINE_US: u32 = 2_000_000;

/// 运行编解码回环。全部帧解码成功、亮度色度都对得上才算通过。
pub fn run(width: u32, height: u32, fps: u32, seconds: u64) -> Result<(), String> {
    let _com = rc_win32core::ComApartment::init(rc_win32core::ApartmentModel::MultiThreaded)?;
    rc_platform_win::register_all();
    let shared =
        rc_d3d11::SharedDevice::get().map_err(|err| format!("共享 D3D11 设备不可用：{err}"))?;
    let frames = (fps * seconds as u32).clamp(1, 600);
    println!(
        "== 编解码回环 ==\n{width}x{height} @ {fps} fps · {frames} 帧 · H.264 8 Mbps · 合成灰阶帧"
    );

    let mut encoder = rc_encode_mf::MfEncoder::new(EncoderConfig {
        resolution: Resolution::new(width, height),
        fps,
        bitrate_bps: 8_000_000,
        ..EncoderConfig::default()
    })
    .map_err(|err| format!("编码器创建失败：{err}"))?;
    let mut decoder = rc_decode_mf::new_decoder(rc_decode_mf::MfDecoderConfig {
        codec: rc_encode::Codec::H264,
        max_resolution: Resolution::new(4096, 4096),
    })
    .map_err(|err| format!("解码器创建失败：{err}"))?;

    // 合成帧源：一张 NV12 纹理，CPU 侧填好灰阶后整帧上传
    let source = shared
        .create_nv12_texture(width, height)
        .map_err(|err| format!("创建合成帧纹理失败：{err}"))?;
    let source_resource: ID3D11Resource = source
        .cast()
        .map_err(|err| format!("合成帧纹理转资源失败：{err}"))?;
    let context = shared.context();
    let mut pixels = Pixels::default();

    let (mut encoded_bytes, mut max_encode, mut max_decode) =
        (0usize, Duration::ZERO, Duration::ZERO);
    let (mut decoded, mut mismatches) = (0u32, 0u32);
    let started = Instant::now();
    for index in 0..frames {
        let luma = 16 + index * 219 / frames;
        // NV12 两个平面行距相同：Y 平面 width × height，UV 交错平面 width × height / 2
        let mut plane = vec![luma as u8; (width * height) as usize];
        plane.resize((width * height * 3 / 2) as usize, 128);
        unsafe {
            context.UpdateSubresource(&source_resource, 0, None, plane.as_ptr().cast(), width, 0)
        };
        // 截止时间与帧时间戳同属一个时钟域（主机时钟），与管线里的用法一致
        let frame = rc_capture::CapturedFrame {
            texture: rc_capture::FrameTexture::D3D11(Interface::as_raw(&source)),
            timestamp: Timestamp(rc_win32core::host_time_us() as u32),
            dirty: None,
            seq: rc_types::CaptureSeq(index as u64 + 1),
            is_repeat: false,
            resource_guard: None,
        };
        let deadline = Timestamp(frame.timestamp.0 + FRAME_DEADLINE_US);

        let encode_started = Instant::now();
        let encoded = encoder
            .encode(&frame, index == 0, deadline)
            .map_err(|err| format!("第 {} 帧编码失败：{err}", index + 1))?;
        max_encode = max_encode.max(encode_started.elapsed());
        encoded_bytes += encoded.data.len();

        let decode_started = Instant::now();
        let output = decoder
            .decode(&encoded.data, encoded.kind, deadline)
            .map_err(|err| format!("第 {} 帧解码失败：{err}", index + 1))?;
        max_decode = max_decode.max(decode_started.elapsed());
        let decoded_frame = match output {
            DecodeOutput::Frame(frame) => frame,
            DecodeOutput::NeedKeyframe => {
                return Err(format!(
                    "第 {} 帧解码端仍在等关键帧（编码端标记关键帧 = {}）",
                    index + 1,
                    encoded.is_keyframe
                ));
            }
            DecodeOutput::Nothing => {
                return Err(format!(
                    "第 {} 帧解码端没有交出画面（{} 字节 {}）",
                    index + 1,
                    encoded.data.len(),
                    frame_kind(&encoded)
                ));
            }
        };
        decoded += 1;
        let decoded_texture = texture_of(&decoded_frame)?;
        let (actual_luma, actual_chroma) = pixels.sample(&shared, &decoded_texture)?;
        let luma_ok = (actual_luma as i32 - luma as i32).abs() <= LUMA_TOLERANCE;
        let chroma_ok = (actual_chroma as i32 - 128).abs() <= CHROMA_TOLERANCE;
        if index < 3 {
            let (source_luma, source_chroma) = pixels.sample(&shared, &source)?;
            println!(
                "  第 {} 帧：源读回 亮度 {source_luma} 色度 {source_chroma} → {} 字节 {} → 解出 亮度 {actual_luma} 色度 {actual_chroma}",
                index + 1,
                encoded.data.len(),
                frame_kind(&encoded)
            );
        }
        if !luma_ok || !chroma_ok {
            mismatches += 1;
            if mismatches <= 5 {
                println!(
                    "  第 {} 帧：应为 亮度 {luma} 色度 128，解出 亮度 {actual_luma} 色度 {actual_chroma}（{}）",
                    index + 1,
                    frame_kind(&encoded)
                );
            }
        }
    }
    let elapsed = started.elapsed();
    println!(
        "编码 {frames} 帧共 {encoded_bytes} 字节（平均 {:.0} kbps）· 解码 {decoded} 帧 · 像素不符 {mismatches} 帧",
        encoded_bytes as f64 * 8.0 / 1000.0 / elapsed.as_secs_f64().max(0.001),
    );
    println!(
        "耗时：单帧编码最长 {:.2} ms · 解码最长 {:.2} ms · 实测 {:.1} fps",
        max_encode.as_secs_f64() * 1000.0,
        max_decode.as_secs_f64() * 1000.0,
        frames as f64 / elapsed.as_secs_f64()
    );
    if decoded != frames {
        return Err(format!("只解出 {decoded}/{frames} 帧"));
    }
    if mismatches > 0 {
        return Err(format!(
            "{mismatches} 帧解出的像素与源不符（亮度误差 > {LUMA_TOLERANCE} 或色度误差 > {CHROMA_TOLERANCE}）"
        ));
    }
    println!("通过：编解码链路正常");
    Ok(())
}

fn frame_kind(frame: &EncodedFrame) -> &'static str {
    match frame.kind {
        FrameKind::Idr => "IDR",
        FrameKind::P => "P 帧",
    }
}

/// 取解码帧的纹理（借用，不接管引用计数）。
fn texture_of(frame: &rc_decode::DecodedFrame) -> Result<ID3D11Texture2D, String> {
    let rc_capture::FrameTexture::D3D11(raw) = frame.texture else {
        return Err("解码帧不是 D3D11 纹理".to_string());
    };
    // 解码器的纹理环持有纹理，这里 Clone 一份引用只是为了读回期间保活
    let borrowed =
        unsafe { ID3D11Texture2D::from_raw_borrowed(&raw) }.ok_or("解码帧纹理指针为空")?;
    Ok(borrowed.clone())
}

/// NV12 纹理像素读回：经一块暂存纹理拷回 CPU，取指定位置的亮度与 U。
#[derive(Default)]
struct Pixels {
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

impl Pixels {
    /// 取纹理中心像素的（亮度，U）。
    fn sample(
        &mut self,
        shared: &rc_d3d11::SharedDevice,
        texture: &ID3D11Texture2D,
    ) -> Result<(u8, u8), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let (width, height) = (desc.Width, desc.Height);
        let context = shared.context();
        let staging = match &self.staging {
            Some((staging, w, h)) if (*w, *h) == (width, height) => staging.clone(),
            _ => {
                let mut staging = None;
                unsafe {
                    shared.device().CreateTexture2D(
                        &D3D11_TEXTURE2D_DESC {
                            Width: width,
                            Height: height,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: DXGI_FORMAT_NV12,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Usage: D3D11_USAGE_STAGING,
                            BindFlags: 0,
                            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                            MiscFlags: 0,
                        },
                        None,
                        Some(&mut staging),
                    )
                }
                .map_err(|err| format!("创建读回暂存纹理失败：{err}"))?;
                let staging = staging.ok_or("读回暂存纹理未返回")?;
                self.staging = Some((staging.clone(), width, height));
                staging
            }
        };
        let source: ID3D11Resource = texture
            .cast()
            .map_err(|err| format!("源纹理转资源失败：{err}"))?;
        let target: ID3D11Resource = staging
            .cast()
            .map_err(|err| format!("暂存纹理转资源失败：{err}"))?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.CopyResource(&target, &source);
            context
                .Map(&target, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|err| format!("读回映射失败：{err}"))?;
            let valid = !mapped.pData.is_null() && mapped.RowPitch as usize >= width as usize;
            let sample = if !valid {
                context.Unmap(&target, 0);
                return Err(format!(
                    "读回映射无效：指针空 {} · 行距 {}（宽 {width}）",
                    mapped.pData.is_null(),
                    mapped.RowPitch
                ));
            } else {
                let base = mapped.pData as *const u8;
                let pitch = mapped.RowPitch as usize;
                let (x, y) = ((width / 2) as usize, (height / 2) as usize);
                // NV12：Y 平面在前，UV 交错平面紧随其后（行距相同），U 在偶数字节
                let luma = *base.add(y * pitch + x);
                let chroma = *base.add(pitch * height as usize + y / 2 * pitch + (x & !1));
                (luma, chroma)
            };
            context.Unmap(&target, 0);
            Ok(sample)
        }
    }
}

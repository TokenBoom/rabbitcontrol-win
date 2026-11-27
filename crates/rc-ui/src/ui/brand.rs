//! 品牌图标：解析 `app.ico` 的 32 位 BMP 帧为 [`WriteableBitmap`]，供标题栏
//! 与登录页展示真实 logo。
//!
//! 图标母版是 `crates/rc-ui/app-icon-source.png`（设计方交付的 256×256 原图），
//! `scripts/make-icon.py` 把它 1:1 封成多尺寸 ICO（256 帧原样、小尺寸降采样）。
//! 这里按目标显示尺寸挑一张「不小于目标的最小帧」，各 DPI 下都是清晰像素。
//!
//! 走纯 Rust 像素拷贝 + `IBufferByteAccess` 直写，全程同步——STA UI 线程
//! 禁用 `IAsyncOperation::get()` 阻塞，故不用 BitmapImage + DataWriter 路径。

#![allow(unsafe_code)] // 像素缓冲直写

use windows::core::Interface;
use windows::UI::Xaml::Media::Imaging::WriteableBitmap;

/// 仓库内图标字节（build.rs 同步嵌入 exe 资源，资源 ID 1）。
pub const ICON_BYTES: &[u8] = include_bytes!("../../app.ico");

/// ICO 目录中的一帧 32bpp BMP。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameEntry {
    width: u32,
    height: u32,
    offset: usize,
}

/// 读取 ICO 目录中的全部 32bpp BMP 帧（PNG 帧跳过），按面积升序。
///
/// 目录项的宽高各占一个字节，`0` 表示 256（256 帧的编码方式）。
fn bmp32_frames(bytes: &[u8]) -> Option<Vec<FrameEntry>> {
    let header = bytes.get(..6)?;
    let (reserved, kind) = (
        u16::from_le_bytes([header[0], header[1]]),
        u16::from_le_bytes([header[2], header[3]]),
    );
    if reserved != 0 || kind != 1 {
        return None;
    }
    let count = u16::from_le_bytes([header[4], header[5]]) as usize;
    let mut frames = Vec::new();
    for i in 0..count {
        let e = bytes.get(6 + i * 16..6 + i * 16 + 16)?;
        let dim = |v: u8| if v == 0 { 256 } else { u32::from(v) };
        let bpp = u16::from_le_bytes([e[6], e[7]]);
        let offset = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        // PNG 帧签名：不能按 BMP 解
        let is_png = bytes.get(offset..offset + 4) == Some(&[0x89, 0x50, 0x4E, 0x47]);
        if bpp != 32 || is_png {
            continue;
        }
        frames.push(FrameEntry {
            width: dim(e[0]),
            height: dim(e[1]),
            offset,
        });
    }
    if frames.is_empty() {
        return None;
    }
    frames.sort_by_key(|f| f.width * f.height);
    Some(frames)
}

/// 解码一帧：40 字节 BITMAPINFOHEADER + BGRA 行序自底向上（AND mask 在尾部，忽略）。
///
/// 输出 BGRA 自上而下、α 已预乘（`WriteableBitmap` 是 Pbgra32）。
fn decode_frame(bytes: &[u8], frame: FrameEntry) -> Option<(u32, u32, Vec<u8>)> {
    let FrameEntry {
        width,
        height,
        offset,
    } = frame;
    let header = 40;
    let stride = width as usize * 4;
    let data = bytes.get(offset + header..offset + header + stride * height as usize)?;
    let mut out = vec![0u8; data.len()];
    for row in 0..height as usize {
        let src_row = height as usize - 1 - row;
        out[row * stride..(row + 1) * stride]
            .copy_from_slice(&data[src_row * stride..(src_row + 1) * stride]);
    }
    // 直通 α → 预乘
    for px in out.chunks_exact_mut(4) {
        let a = u16::from(px[3]);
        if a < 255 {
            px[0] = (u16::from(px[0]) * a / 255) as u8;
            px[1] = (u16::from(px[1]) * a / 255) as u8;
            px[2] = (u16::from(px[2]) * a / 255) as u8;
        }
    }
    Some((width, height, out))
}

/// 测试用：解码 ICO 中最大的一帧 → `(宽, 高, BGRA 自上而下像素)`。
#[cfg(test)]
fn decode_largest_bmp32(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let frames = bmp32_frames(bytes)?;
    decode_frame(bytes, *frames.last()?)
}

/// 选一张「不小于 `min_px` 的最小帧」；都不够大时取最大帧。
fn select_frame(bytes: &[u8], min_px: u32) -> Option<FrameEntry> {
    let frames = bmp32_frames(bytes)?;
    frames
        .iter()
        .copied()
        .find(|f| f.width >= min_px)
        .or_else(|| frames.last().copied())
}

/// 按目标显示尺寸（物理像素）解码最合适的帧 → `(宽, 高, BGRA 自上而下像素)`。
pub fn decode_bmp32_for(bytes: &[u8], min_px: u32) -> Option<(u32, u32, Vec<u8>)> {
    decode_frame(bytes, select_frame(bytes, min_px)?)
}

/// 构建目标显示尺寸（物理像素）的图标位图；解码失败返回 `None`（调用方保留
/// XAML 占位）。
pub fn icon_bitmap(min_px: u32) -> Option<WriteableBitmap> {
    let (w, h, pixels) = decode_bmp32_for(ICON_BYTES, min_px)?;
    let bitmap = WriteableBitmap::CreateInstanceWithDimensions(w as i32, h as i32).ok()?;
    let buffer = bitmap.PixelBuffer().ok()?;
    let access: windows::Win32::System::WinRT::IBufferByteAccess = buffer.cast().ok()?;
    unsafe {
        let dst = access.Buffer().ok()?;
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), dst, pixels.len());
    }
    Some(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_at(pixels: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * w + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    }

    #[test]
    fn ico_carries_every_expected_bmp_frame() {
        // 由 scripts/make-icon.py 生成：10 个 32bpp BMP 帧，16 到 256
        let widths: Vec<u32> = bmp32_frames(ICON_BYTES)
            .expect("应解析出帧目录")
            .iter()
            .map(|f| f.width)
            .collect();
        assert_eq!(widths, [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]);
    }

    #[test]
    fn largest_frame_is_the_256_master_verbatim() {
        // 256 帧必须就是设计母版（app-icon-source.png）的原始像素——
        // 钉死三个已知像素，防止图标被人重新绘制后悄悄替换。
        let (w, h, pixels) = decode_largest_bmp32(ICON_BYTES).expect("最大帧");
        assert_eq!((w, h), (256, 256));
        assert_eq!(pixels.len(), 256 * 256 * 4);
        let w = w as usize;
        // 圆角外的透明角
        assert_eq!(bgra_at(&pixels, w, 0, 0), [0, 0, 0, 0]);
        // 蓝色渐变底 (41,108,203)
        assert_eq!(bgra_at(&pixels, w, 128, 128), [203, 108, 41, 255]);
        // 白色兔身
        assert_eq!(bgra_at(&pixels, w, 150, 150), [255, 255, 255, 255]);
    }

    #[test]
    fn partial_alpha_is_premultiplied() {
        // 母版投影处 (23,128) = RGB(0,14,57) α=18 → 预乘后 B=4, G=0, R=0
        let (w, _, pixels) = decode_largest_bmp32(ICON_BYTES).expect("最大帧");
        assert_eq!(bgra_at(&pixels, w as usize, 23, 128), [4, 0, 0, 18]);
    }

    #[test]
    fn frame_selection_prefers_smallest_sufficient() {
        let width_of = |min_px: u32| decode_bmp32_for(ICON_BYTES, min_px).map(|(w, _, _)| w);
        assert_eq!(width_of(0), Some(16));
        assert_eq!(width_of(36), Some(40));
        assert_eq!(width_of(128), Some(128));
        // 超出最大帧：取最大
        assert_eq!(width_of(9999), Some(256));
        // 选中的帧尺寸必须覆盖目标
        let (w, h, pixels) = decode_bmp32_for(ICON_BYTES, 100).expect("帧");
        assert_eq!((w, h), (128, 128));
        assert_eq!(pixels.len(), 128 * 128 * 4);
    }

    #[test]
    fn ico_decode_rejects_garbage() {
        assert!(decode_largest_bmp32(&[]).is_none());
        assert!(decode_largest_bmp32(&[0u8; 8]).is_none());
        // 声明了一帧但数据被截断
        let mut truncated = vec![0, 0, 1, 0, 0, 0];
        truncated.extend_from_slice(&[16, 16, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 22, 0, 0, 0]);
        assert!(decode_largest_bmp32(&truncated).is_none());
    }
}

//! 品牌图标：解析 `app.ico` 的 32 位 BMP 帧为 [`WriteableBitmap`]，供标题栏
//! 与登录页展示真实 logo。
//!
//! 走纯 Rust 像素拷贝 + `IBufferByteAccess` 直写，全程同步——STA UI 线程
//! 禁用 `IAsyncOperation::get()` 阻塞，故不用 BitmapImage + DataWriter 路径。

#![allow(unsafe_code)] // 像素缓冲直写

use windows::core::Interface;
use windows::UI::Xaml::Media::Imaging::WriteableBitmap;

/// 仓库内图标字节（build.rs 同步嵌入 exe 资源，资源 ID 1）。
pub const ICON_BYTES: &[u8] = include_bytes!("../../app.ico");

/// 解析 ICO 中最大的一帧 32bpp BMP 帧 → `(宽, 高, BGRA 自上而下像素)`。
///
/// 仅支持 BMP 编码帧（PNG 帧跳过）：与 `app.ico` 现状匹配；未来换成
/// PNG-only ico 时需改走 BitmapDecoder（异步）路径。
pub fn decode_largest_bmp32(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if bytes.len() < 6 || u16::from_le_bytes([bytes[0], bytes[1]]) != 0 {
        return None;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut best: Option<(usize, u8, u8, usize)> = None; // (面积, w, h, 数据偏移)
    for i in 0..count {
        let base = 6 + i * 16;
        let e = bytes.get(base..base + 16)?;
        let (w, h) = (e[0], e[1]);
        let bpp = u16::from_le_bytes([e[6], e[7]]);
        let offset = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        if bpp != 32 || w == 0 || h == 0 {
            continue;
        }
        // PNG 帧签名：不能按 BMP 解
        if bytes.get(offset..offset + 4) == Some(&[0x89, 0x50, 0x4E, 0x47]) {
            continue;
        }
        let area = w as usize * h as usize;
        if best.map(|(a, _, _, _)| area > a).unwrap_or(true) {
            best = Some((area, w, h, offset));
        }
    }
    let (_, w, h, offset) = best?;

    // BMP 帧：40 字节 BITMAPINFOHEADER + BGRA 行序自底向上（AND mask 在尾部，忽略）
    let header = 40;
    let stride = w as usize * 4;
    let data = bytes.get(offset + header..offset + header + stride * h as usize)?;
    let mut out = vec![0u8; data.len()];
    for row in 0..h as usize {
        let src_row = h as usize - 1 - row;
        out[row * stride..(row + 1) * stride]
            .copy_from_slice(&data[src_row * stride..(src_row + 1) * stride]);
    }
    // 直通 α → 预乘（WriteableBitmap 是 Pbgra32）
    for px in out.chunks_exact_mut(4) {
        let a = px[3] as u16;
        if a < 255 {
            px[0] = (px[0] as u16 * a / 255) as u8;
            px[1] = (px[1] as u16 * a / 255) as u8;
            px[2] = (px[2] as u16 * a / 255) as u8;
        }
    }
    Some((w as u32, h as u32, out))
}

/// 构建图标位图；解码失败返回 `None`（调用方保留 XAML 占位）。
pub fn icon_bitmap() -> Option<WriteableBitmap> {
    let (w, h, pixels) = decode_largest_bmp32(ICON_BYTES)?;
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

    #[test]
    fn ico_decodes_largest_frame() {
        // 仓库自带 app.ico：单帧 32×32 32bpp BMP
        let (w, h, pixels) = decode_largest_bmp32(ICON_BYTES).expect("应解析出 BMP32 帧");
        assert_eq!((w, h), (32, 32));
        assert_eq!(pixels.len(), 32 * 32 * 4);
    }

    #[test]
    fn ico_decode_rejects_garbage() {
        assert!(decode_largest_bmp32(&[]).is_none());
        assert!(decode_largest_bmp32(&[0u8; 8]).is_none());
    }
}

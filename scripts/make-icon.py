#!/usr/bin/env python3
"""从设计母版生成 app.ico（像素级 1:1，不重绘、不改形、不改色）。

母版：crates/rc-ui/app-icon-source.png（256×256 RGBA，设计方交付的原图）。
产物：crates/rc-ui/app.ico —— 多尺寸 ICO，全部 32bpp BMP 帧（含 AND 掩码）。

规则：
- 256 帧直接使用母版像素，不做任何重采样；
- 其余尺寸由母版降采样得到（Lanczos，先预乘 alpha 再重采样，避免透明
  边缘出现黑边/彩边）；
- ICO 容器按规范手写（Pillow 的 ICO 写盘会漏 AND 掩码、且跳过比基图大的
  帧）：每帧 = 40 字节 BITMAPINFOHEADER（高度 ×2）+ 自底向上 BGRA + AND 掩码；
- 全部写 BMP 帧（而非 PNG 帧）：rc-ui 的 brand.rs 只解析 BMP 帧，供
  标题栏 / 登录页按目标尺寸挑选最合适的帧。

用法：python scripts/make-icon.py
"""

import os
import struct
import sys

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
SOURCE = os.path.join(ROOT, "crates", "rc-ui", "app-icon-source.png")
OUTPUT = os.path.join(ROOT, "crates", "rc-ui", "app.ico")

# 覆盖 100%–300% 缩放下的任务栏 / 标题栏 / 资源管理器常见尺寸。
SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
HEADER = struct.Struct("<IiiHHIIiiII")  # BITMAPINFOHEADER


def downscale(master: Image.Image, size: int) -> Image.Image:
    """母版降采样到 size×size（预乘 alpha 保证边缘干净）。"""
    return (
        master.convert("RGBa")
        .resize((size, size), Image.LANCZOS, reducing_gap=3.0)
        .convert("RGBA")
    )


def dib_frame(rgba: bytes, width: int, height: int) -> bytes:
    """把自顶向下的 RGBA 像素封成 ICO 的 32bpp BMP 帧。"""
    stride = width * 4
    # XOR 位图：BGRA、自底向上
    rows = []
    for y in range(height - 1, -1, -1):
        row = rgba[y * stride : (y + 1) * stride]
        rows.append(b"".join(row[x : x + 4][2::-1] + row[x + 3 : x + 4] for x in range(0, stride, 4)))
    xor = b"".join(rows)
    # AND 掩码：1bpp、每行按 4 字节对齐、自底向上；1 = 透明
    mask_stride = ((width + 31) // 32) * 4
    and_rows = []
    for y in range(height - 1, -1, -1):
        row = bytearray(mask_stride)
        for x in range(width):
            if rgba[(y * width + x) * 4 + 3] == 0:
                row[x // 8] |= 0x80 >> (x % 8)
        and_rows.append(bytes(row))
    and_mask = b"".join(and_rows)
    header = HEADER.pack(
        HEADER.size, width, height * 2, 1, 32, 0, len(xor) + len(and_mask),
        0, 0, 0, 0,
    )
    return header + xor + and_mask


def main() -> int:
    master = Image.open(SOURCE).convert("RGBA")
    if master.size != (256, 256):
        print(f"母版尺寸应为 256×256，实际 {master.size}", file=sys.stderr)
        return 1

    frames = [master if size == 256 else downscale(master, size) for size in SIZES]
    blobs = [dib_frame(frame.tobytes(), size, size) for frame, size in zip(frames, SIZES)]

    out = bytearray(struct.pack("<HHH", 0, 1, len(SIZES)))
    offset = 6 + 16 * len(SIZES)
    for size, blob in zip(SIZES, blobs):
        out += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    for blob in blobs:
        out += blob

    with open(OUTPUT, "wb") as fp:
        fp.write(out)
    print(
        f"已生成 {os.path.relpath(OUTPUT, ROOT)}（{len(out)} 字节，"
        f"帧：{'/'.join(str(s) for s in SIZES)}）"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

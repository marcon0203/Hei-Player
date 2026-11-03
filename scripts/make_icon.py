#!/usr/bin/env python3
"""生成 AuraPlay 应用图标(1024 RGBA PNG,纯标准库):深色圆角底 + 青→紫渐变波形条。"""
import struct
import sys
import zlib

S = 1024
R = 190  # 圆角半径


def px(x, y):
    """返回 (r,g,b,a)。"""
    # 圆角掩码(SDF 近似)
    cx = min(max(x, R), S - R)
    cy = min(max(y, R), S - R)
    inside = (x - cx) ** 2 + (y - cy) ** 2 <= R**2

    r, g, b, a = 16, 19, 26, 0
    if inside:
        a = 255
        # 背景纵向微渐变
        r, g, b = 14 + y * 14 // S, 17 + y * 16 // S, 24 + y * 24 // S
        # 描边
        edge = (x < 8 or y < 8 or x > S - 9 or y > S - 9) or (
            (x - cx) ** 2 + (y - cy) ** 2 > (R - 9) ** 2
            and (min(max(x, R), S - R) != x or min(max(y, R), S - R) != y or True)
        )
        # 波形条:9 根,中心横排在 y≈512
        heights = [0.22, 0.42, 0.66, 0.92, 0.55, 0.82, 0.38, 0.62, 0.28]
        bw, gap = 54, 34
        total = len(heights) * bw + (len(heights) - 1) * gap
        x0 = (S - total) // 2
        for i, h in enumerate(heights):
            bx0 = x0 + i * (bw + gap)
            if bx0 <= x < bx0 + bw:
                half = h * 330
                if 512 - half <= y < 512 + half:
                    t = (x - bx0) / bw                      # 条内横向
                    t2 = (y - (512 - half)) / (2 * half)    # 条内纵向
                    # 青(34,211,238) → 紫(124,92,255)
                    r = int(34 + (124 - 34) * t)
                    g = int(211 + (92 - 211) * t)
                    b = int(238 + (255 - 238) * t)
                    # 上下端渐暗,做出圆头效果
                    k = min(t2, 1 - t2) / 0.5
                    k = 0.55 + 0.45 * (1 - (1 - min(k / 0.35, 1)) ** 2) if k < 0.35 else 1.0
                    r, g, b = int(r * k), int(g * k), int(b * k)
        if edge and not (y == 511 or y == 512):
            # 简化:外圈 6px 半透明描边
            if (x < 6 or y < 6 or x > S - 7 or y > S - 7) or (
                (x - cx) ** 2 + (y - cy) ** 2 > (R - 6) ** 2
            ):
                r, g, b = 42, 52, 68
    return r, g, b, a


def write_png(path):
    raw = bytearray()
    for y in range(S):
        raw.append(0)  # filter: none
        for x in range(S):
            raw.extend(px(x, y))

    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c))

    ihdr = struct.pack(">IIBBBBB", S, S, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as f:
        f.write(png)


if __name__ == "__main__":
    write_png(sys.argv[1] if len(sys.argv) > 1 else "icon_1024.png")
    print("icon written")

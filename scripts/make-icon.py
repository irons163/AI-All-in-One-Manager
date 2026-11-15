#!/usr/bin/env python3
"""Draw the AIOM app icon (1024x1024 RGBA PNG) with nothing but the standard library.

A rounded square with a diagonal indigo-to-violet gradient and three
overlapping discs: several accounts, one place.

Usage: python3 scripts/make-icon.py app/icons/icon.png
"""
import struct
import sys
import zlib

SIZE = 1024
HALF = 412  # half the side of the rounded square (leaves the standard margin)
RADIUS = 185
TOP = (79, 70, 229)  # indigo
BOTTOM = (147, 51, 234)  # violet
DISCS = [  # (cx, cy, radius, opacity)
    (512, 405, 150, 0.95),
    (404, 592, 150, 0.62),
    (620, 592, 150, 0.38),
]


def clamp(value):
    return 0.0 if value < 0.0 else 1.0 if value > 1.0 else value


def pixel(x, y):
    px, py = x + 0.5 - SIZE / 2, y + 0.5 - SIZE / 2
    qx, qy = abs(px) - (HALF - RADIUS), abs(py) - (HALF - RADIUS)
    outside = (max(qx, 0.0) ** 2 + max(qy, 0.0) ** 2) ** 0.5
    distance = outside + min(max(qx, qy), 0.0) - RADIUS
    alpha = clamp(0.5 - distance)
    if alpha == 0.0:
        return 0, 0, 0, 0

    t = clamp((px + py) / (4 * HALF) + 0.5)
    r, g, b = (TOP[i] + (BOTTOM[i] - TOP[i]) * t for i in range(3))
    for cx, cy, radius, opacity in DISCS:
        d = ((x + 0.5 - cx) ** 2 + (y + 0.5 - cy) ** 2) ** 0.5 - radius
        cover = clamp(0.5 - d) * opacity
        if cover:
            r, g, b = (c + (255 - c) * cover for c in (r, g, b))
    return round(r), round(g), round(b), round(alpha * 255)


def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)


def main(out_path):
    rows = bytearray()
    for y in range(SIZE):
        rows.append(0)  # filter type: none
        for x in range(SIZE):
            rows.extend(pixel(x, y))
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(rows), 9))
        + chunk(b"IEND", b"")
    )
    with open(out_path, "wb") as handle:
        handle.write(png)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "icon.png")

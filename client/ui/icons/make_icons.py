#!/usr/bin/env python3
"""Draws the PingPath icon set with the Python standard library only.

    python3 client/ui/icons/make_icons.py

writes, next to this script:

- icon.ico: PNG frames of 16, 24, 32, 48, 64 and 256 pixels (Windows Vista
  and later read PNG frames; this is the layout `tauri icon` produces). The
  window, the tray and the installer use it.
- 32x32.png, 128x128.png and icon.png (512 pixels), which tauri.conf.json
  lists for the other platforms.

The design: a rounded teal square with a white route between two points,
the start low on the left and the end high on the right, for "a better
path". It is drawn from geometry rather than pixels so every size stays
sharp: each pixel is 4 x 4 samples, so edges are smoothed without any image
library. The script is deterministic; rerun it after changing the design and
commit the outputs.
"""

import math
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))

TOP = (0x14, 0x9E, 0xA8)  # background, top
BOTTOM = (0x0B, 0x5E, 0x75)  # background, bottom
WHITE = (0xFF, 0xFF, 0xFF)

# The route, in units of the icon's side: a polyline and its two end points.
ROUTE = [(0.24, 0.72), (0.44, 0.50), (0.56, 0.60), (0.76, 0.30)]
STROKE = 0.075
DOT = 0.095
CORNER = 0.22  # corner radius of the background square
SAMPLES = 4


def rounded_square(u, v):
    """Whether (u, v) in [0, 1]^2 is inside the rounded background."""
    r = CORNER
    cu = min(max(u, r), 1 - r)
    cv = min(max(v, r), 1 - r)
    return (u - cu) ** 2 + (v - cv) ** 2 <= r * r


def segment_distance(p, a, b):
    (px, py), (ax, ay), (bx, by) = p, a, b
    dx, dy = bx - ax, by - ay
    t = ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)
    t = min(max(t, 0.0), 1.0)
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def on_route(u, v):
    if any(math.hypot(u - x, v - y) <= DOT for x, y in (ROUTE[0], ROUTE[-1])):
        return True
    return any(
        segment_distance((u, v), ROUTE[i], ROUTE[i + 1]) <= STROKE / 2
        for i in range(len(ROUTE) - 1)
    )


def pixel(x, y, n):
    """Straight (not premultiplied) RGBA for pixel (x, y) of an n x n icon."""
    inside = route = 0
    for sy in range(SAMPLES):
        for sx in range(SAMPLES):
            u = (x + (sx + 0.5) / SAMPLES) / n
            v = (y + (sy + 0.5) / SAMPLES) / n
            if rounded_square(u, v):
                inside += 1
                if on_route(u, v):
                    route += 1
    total = SAMPLES * SAMPLES
    if inside == 0:
        return (0, 0, 0, 0)
    t = (y + 0.5) / n
    background = tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM))
    share = route / inside
    color = tuple(round(bg + (w - bg) * share) for bg, w in zip(background, WHITE))
    return color + (round(255 * inside / total),)


def png(n):
    raw = bytearray()
    for y in range(n):
        raw.append(0)  # filter: none
        for x in range(n):
            raw.extend(pixel(x, y, n))

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def ico(frames):
    """An .ico holding PNG frames, given as {size: png bytes}."""
    sizes = sorted(frames)
    out = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    for size in sizes:
        data = frames[size]
        side = 0 if size >= 256 else size  # 0 means 256 in the directory
        out += struct.pack("<BBBBHHII", side, side, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return out + b"".join(frames[size] for size in sizes)


def main():
    cache = {}

    def frame(size):
        if size not in cache:
            cache[size] = png(size)
        return cache[size]

    outputs = {
        "icon.ico": ico({size: frame(size) for size in (16, 24, 32, 48, 64, 256)}),
        "32x32.png": frame(32),
        "128x128.png": frame(128),
        "icon.png": frame(512),
    }
    for name, data in outputs.items():
        with open(os.path.join(HERE, name), "wb") as f:
            f.write(data)
        print(f"wrote {name} ({len(data)} bytes)")


if __name__ == "__main__":
    main()

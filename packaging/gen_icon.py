#!/usr/bin/env python3
"""SoundRouter app icon: two interlocked rings with waveform bars. Stdlib only.

Usage: gen_icon.py OUT.png   (writes a 1024x1024 RGBA PNG)
Anti-aliasing is analytic: each shape is a signed distance field (SDF), coverage = clamp(0.5 - d).
"""
import math
import struct
import sys
import zlib

N = 1024
C = N / 2
TILE_HALF = 412          # 824px tile on the 1024 canvas (Apple icon grid)
TILE_R = 0.224 * 2 * TILE_HALF
RING_R, RING_W = 190.0, 84.0
RING_DX = 125.0
HALO = 12.0              # gap cut in the under-ring where rings cross
LX, RX = C - RING_DX, C + RING_DX
TEAL = ((64, 224, 205), (13, 148, 136))
CORAL = ((255, 125, 128), (238, 64, 72))
# waveform bars: (x offset from ring centre; sign flips it per ring, half-height)
BAR_W = 26.0
BARS = [(-100, 38), (-56, 74), (-12, 38)]


def clamp01(v):
    return 0.0 if v < 0 else 1.0 if v > 1 else v


def cov(d):
    return clamp01(0.5 - d)


def lerp(a, b, t):
    return tuple(x + (y - x) * t for x, y in zip(a, b))


def rrect(x, y, cx, cy, half, r):
    qx, qy = abs(x - cx) - (half - r), abs(y - cy) - (half - r)
    return math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - r


def ring(x, y, cx):
    return abs(math.hypot(x - cx, y - C) - RING_R) - RING_W / 2


def bars(x, y, cx, sign):
    d = 1e9
    for off, hh in BARS:
        bx = cx + sign * off
        dy = max(abs(y - C) - hh, 0)
        d = min(d, math.hypot(x - bx, dy) - BAR_W / 2)
    return d


def over(dst, rgb, a):
    """Composite straight-colour src (rgb, a) over dst (r, g, b, a)."""
    if a <= 0:
        return dst
    oa = a + dst[3] * (1 - a)
    k = dst[3] * (1 - a)
    return tuple((rgb[i] * a + dst[i] * k) / oa for i in range(3)) + (oa,)


def pixel(x, y):
    px = (0.0, 0.0, 0.0, 0.0)
    # soft drop shadow
    ds = rrect(x, y, C, C + 14, TILE_HALF, TILE_R)
    if ds < 60:
        px = over(px, (20, 30, 45), 0.22 * clamp01(1 - ds / 60) ** 2)
    dt = rrect(x, y, C, C, TILE_HALF, TILE_R)
    a = cov(dt)
    if a <= 0:
        return px
    t = clamp01((y - (C - TILE_HALF)) / (2 * TILE_HALF))
    bg = lerp((255, 255, 255), (226, 233, 240), t)
    px = over(px, bg, a)
    # hairline edge
    px = over(px, (160, 172, 186), 0.5 * a * clamp01(1 - abs(dt + 1.0)))

    dl, dr = ring(x, y, LX), ring(x, y, RX)
    top = y < C  # left ring is over on top half, right ring over on bottom half
    d_over, d_under = (dl, dr) if top else (dr, dl)
    # under-ring is cut by a halo around the over-ring; ring stays crisp
    under_a = cov(d_under) * (1 - cov(d_over - HALO))
    over_a = cov(d_over)
    g = clamp01((y - (C - RING_R - RING_W / 2)) / (2 * RING_R + RING_W))
    teal, coral = lerp(TEAL[0], TEAL[1], g), lerp(CORAL[0], CORAL[1], g)
    c_over, c_under = (teal, coral) if top else (coral, teal)
    px = over(px, c_under, under_a)
    px = over(px, c_over, over_a)
    # waveform bars inside each ring's hole, in the ring's own colour
    for cx, sign, col in ((LX, 1, teal), (RX, -1, coral)):
        b = cov(bars(x, y, cx, sign))
        if b > 0:
            px = over(px, col, b)
    return px


def main(out):
    raw = bytearray()
    for j in range(N):
        raw.append(0)
        for i in range(N):
            r, g, b, a = pixel(i + 0.5, j + 0.5)
            raw += bytes((round(r), round(g), round(b), round(a * 255)))

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data))

    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", N, N, 8, 6, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
           + chunk(b"IEND", b""))
    with open(out, "wb") as f:
        f.write(png)


if __name__ == "__main__":
    main(sys.argv[1])

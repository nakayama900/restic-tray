#!/usr/bin/env python3
"""
Deterministic generator for assets/icon.svg, assets/icon.png (256x256), and assets/icon.ico.

The mark matches `paint_brand_mark` in src/theme/icons.rs: a lime tile (#D4F84A) with an
ink (#0E0E0D) ring that is open at the top right, plus a center dot. Proportions are
fractions of the tile size so the app icon, tray icon and in-app logo stay identical.
Uses only Python standard library (struct, zlib, math).
"""

from __future__ import annotations
import math
import pathlib
import struct
import zlib


def clamp(v: float, lo: float = 0.0, hi: float = 255.0) -> int:
    return int(max(lo, min(hi, round(v))))


def lerp(a: float, b: float, t: float) -> float:
    return a + (b - a) * max(0.0, min(1.0, t))


def blend(dst: tuple[int, int, int, int], src: tuple[int, int, int, float]) -> tuple[int, int, int, int]:
    sr, sg, sb, sa = src
    if sa <= 0.0:
        return dst
    dr, dg, db, da = dst
    da_f = da / 255.0
    out_a = sa + da_f * (1.0 - sa)
    if out_a <= 1e-6:
        return (0, 0, 0, 0)
    r = (sr * sa + dr * da_f * (1.0 - sa)) / out_a
    g = (sg * sa + dg * da_f * (1.0 - sa)) / out_a
    b = (sb * sa + db * da_f * (1.0 - sa)) / out_a
    return (clamp(r), clamp(g), clamp(b), clamp(out_a * 255.0))


def sd_rounded_box(px: float, py: float, cx: float, cy: float, hw: float, hh: float, r: float) -> float:
    qx = abs(px - cx) - hw + r
    qy = abs(py - cy) - hh + r
    return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - r


LIME = (212, 248, 74)
INK = (14, 14, 13)

# Fractions of the tile size, shared with src/theme/icons.rs.
TILE = 488.0
TILE_RADIUS = 0.27 * TILE
RING_RADIUS = 0.25 * TILE
RING_WIDTH = 0.11 * TILE
DOT_RADIUS = 0.08 * TILE
ARC_FROM_DEG = -10.0
ARC_TO_DEG = 280.0


def arc_point(deg: float) -> tuple[float, float]:
    a = math.radians(deg)
    return 256.0 + RING_RADIUS * math.cos(a), 256.0 + RING_RADIUS * math.sin(a)


def sd_open_ring(px: float, py: float) -> float:
    """Signed distance to the ring stroke with round caps. Angles are screen space (y down)."""
    ang = math.degrees(math.atan2(py - 256.0, px - 256.0))
    rel = (ang - ARC_FROM_DEG) % 360.0
    if rel <= ARC_TO_DEG - ARC_FROM_DEG:
        d = abs(math.hypot(px - 256.0, py - 256.0) - RING_RADIUS)
    else:
        d = min(math.hypot(px - x, py - y) for x, y in (arc_point(ARC_FROM_DEG), arc_point(ARC_TO_DEG)))
    return d - RING_WIDTH / 2.0


def render_icon_rgba(size: int) -> bytes:
    scale = size / 512.0
    aa = max(1.0, 1.0 / scale)
    buf = bytearray(size * size * 4)

    for y in range(size):
        py = (y + 0.5) / scale
        for x in range(size):
            px = (x + 0.5) / scale

            d_bg = sd_rounded_box(px, py, 256.0, 256.0, TILE / 2.0, TILE / 2.0, TILE_RADIUS)
            if d_bg > aa:
                continue
            bg_cov = max(0.0, min(1.0, 0.5 - d_bg / aa))
            pixel: tuple[int, int, int, int] = (*LIME, clamp(bg_cov * 255.0))

            d_ink = min(sd_open_ring(px, py), math.hypot(px - 256.0, py - 256.0) - DOT_RADIUS)
            if d_ink < aa:
                cov = max(0.0, min(1.0, 0.5 - d_ink / aa)) * bg_cov
                pixel = blend(pixel, (*INK, cov))

            idx = (y * size + x) * 4
            buf[idx : idx + 4] = bytes(pixel)

    return bytes(buf)


def encode_png(width: int, height: int, rgba: bytes) -> bytes:
    def chunk(tag: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)
        raw.extend(rgba[y * stride : (y + 1) * stride])

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    idat = zlib.compress(bytes(raw), level=9)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")


def encode_ico(sizes: list[int]) -> tuple[bytes, bytes]:
    png_blobs: list[tuple[int, bytes]] = []
    png_256 = b""
    for sz in sizes:
        rgba = render_icon_rgba(sz)
        png = encode_png(sz, sz, rgba)
        png_blobs.append((sz, png))
        if sz == 256:
            png_256 = png

    header = struct.pack("<HHH", 0, 1, len(png_blobs))
    offset = 6 + 16 * len(png_blobs)
    entries = bytearray()
    payload = bytearray()

    for sz, png in png_blobs:
        w_byte = 0 if sz >= 256 else sz
        h_byte = 0 if sz >= 256 else sz
        entries.extend(
            struct.pack(
                "<BBBBHHII",
                w_byte,
                h_byte,
                0,
                0,
                1,
                32,
                len(png),
                offset,
            )
        )
        payload.extend(png)
        offset += len(png)

    return bytes(header + entries + payload), png_256


def svg_markup() -> str:
    (sx, sy), (ex, ey) = arc_point(ARC_FROM_DEG), arc_point(ARC_TO_DEG)
    large_arc = 1 if ARC_TO_DEG - ARC_FROM_DEG > 180.0 else 0
    o = 256.0 - TILE / 2.0
    lines = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" width="512" height="512">',
        f'  <rect x="{o:g}" y="{o:g}" width="{TILE:g}" height="{TILE:g}" rx="{TILE_RADIUS:.2f}" fill="#D4F84A"/>',
        f'  <path d="M{sx:.2f} {sy:.2f} A{RING_RADIUS:g} {RING_RADIUS:g} 0 {large_arc} 1 {ex:.2f} {ey:.2f}" '
        f'stroke="#0E0E0D" stroke-width="{RING_WIDTH:.2f}" stroke-linecap="round" fill="none"/>',
        f'  <circle cx="256" cy="256" r="{DOT_RADIUS:.2f}" fill="#0E0E0D"/>',
        "</svg>",
    ]
    return "".join(line + chr(10) for line in lines)


def main() -> None:
    repo_root = pathlib.Path(__file__).resolve().parent.parent
    assets_dir = repo_root / "assets"
    assets_dir.mkdir(parents=True, exist_ok=True)

    ico_bytes, png_256 = encode_ico([16, 32, 48, 64, 128, 256])
    ico_path = assets_dir / "icon.ico"
    png_path = assets_dir / "icon.png"
    svg_path = assets_dir / "icon.svg"
    ico_path.write_bytes(ico_bytes)
    png_path.write_bytes(png_256)
    svg_path.write_text(svg_markup(), encoding="utf-8")
    print(f"Generated {ico_path} ({len(ico_bytes)}B), {png_path} ({len(png_256)}B), {svg_path}")


if __name__ == "__main__":
    main()

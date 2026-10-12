#!/usr/bin/env python3
"""The admin app's icon: a white cloud on a rounded tile in the brand colour.

Run it from the repository root to draw `assets/favicon.ico` and
`assets/favicon.svg` again:

    python3 apps/admin/favicon.py

Python 3 standard library only (zlib, struct, binascii); the files it writes
are checked in, so nothing needs to run it to build or deploy the app.

Both files exist because `/favicon.ico` is what a browser asks for on its own,
whatever the page says, while the SVG scales to any size and is what the pages
link for browsers that take it. The ICO holds 16x16 and 32x32, each a PNG.

The shape lives on a 32x32 grid: a tile with corner radius 7, and a cloud of
three circles with a flat bottom, kept 2px or more inside the tile so that it
does not clog at 16px. No gradients: at 16px they mean nothing.
"""
import binascii
import os
import struct
import zlib

TILE = (0x34, 0x2A, 0x4D)  # --brand in pages.cove
CLOUD = (0xFF, 0xFF, 0xFF)
GRID = 32
RADIUS = 7
# Wider than it is tall, and the three bumps different sizes: a cloud that
# is as round as the tile reads as a blob at 16px.
CIRCLES = [(9.5, 18.5, 4), (22, 18, 4.5), (15, 15, 5.5)]  # cx, cy, r
FLAT = (9.5, 18, 22, 22.5)  # x0, y0, x1, y1: fills between the side circles


def in_tile(x, y):
    # Distance to the nearest corner centre, only where in a corner square.
    cx = min(max(x, RADIUS), GRID - RADIUS)
    cy = min(max(y, RADIUS), GRID - RADIUS)
    return 0 <= x <= GRID and 0 <= y <= GRID and (x - cx) ** 2 + (y - cy) ** 2 <= RADIUS**2


def in_cloud(x, y):
    x0, y0, x1, y1 = FLAT
    if x0 <= x <= x1 and y0 <= y <= y1:
        return True
    return any((x - cx) ** 2 + (y - cy) ** 2 <= r * r for cx, cy, r in CIRCLES)


def render(size):
    """RGBA rows of `size` x `size`, each pixel 4x4 supersampled."""
    n = 4
    scale = GRID / size
    rows = []
    for py in range(size):
        row = bytearray()
        for px in range(size):
            a = 0
            rgb = [0, 0, 0]
            for sy in range(n):
                for sx in range(n):
                    x = (px + (sx + 0.5) / n) * scale
                    y = (py + (sy + 0.5) / n) * scale
                    if not in_tile(x, y):
                        continue
                    a += 1
                    c = CLOUD if in_cloud(x, y) else TILE
                    for i in range(3):
                        rgb[i] += c[i]
            if a:
                row += bytes(round(v / a) for v in rgb)
            else:
                row += b"\0\0\0"
            row.append(round(255 * a / (n * n)))
        rows.append(bytes(row))
    return rows


def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", binascii.crc32(body) & 0xFFFFFFFF)


def png(size):
    raw = b"".join(b"\0" + row for row in render(size))  # filter type 0 per row
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico(sizes):
    images = [png(s) for s in sizes]
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for s, data in zip(sizes, images):
        out += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return out + b"".join(images)


def svg():
    circles = "".join(f'<circle cx="{cx}" cy="{cy}" r="{r}"/>' for cx, cy, r in CIRCLES)
    x0, y0, x1, y1 = FLAT
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">'
        f'<rect width="32" height="32" rx="{RADIUS}" fill="#342a4d"/>'
        f'<g fill="#ffffff">{circles}<rect x="{x0}" y="{y0}" width="{x1 - x0}" height="{y1 - y0}"/></g>'
        "</svg>\n"
    )


def main():
    here = os.path.join(os.path.dirname(os.path.abspath(__file__)), "assets")
    os.makedirs(here, exist_ok=True)
    with open(os.path.join(here, "favicon.ico"), "wb") as f:
        f.write(ico([16, 32]))
    with open(os.path.join(here, "favicon.svg"), "w") as f:
        f.write(svg())


if __name__ == "__main__":
    main()

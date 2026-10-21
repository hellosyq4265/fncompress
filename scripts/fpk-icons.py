"""Draws the fpk icons (ADR 0012) without third-party modules: a rounded
blue square with a white play triangle between two inward chevrons
("compress"). Writes packaging/fnos/ICON.PNG (64 px), ICON_256.PNG and the
entry icons app/ui/images/icon_64.png, icon_256.png.

    python -I scripts/fpk-icons.py
"""

import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1] / "packaging" / "fnos"
SS = 4  # supersampling per axis


def inside_rounded(x, y, size, radius):
    cx = min(max(x, radius), size - radius)
    cy = min(max(y, radius), size - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius ** 2


def inside_triangle(x, y, pts):
    (x1, y1), (x2, y2), (x3, y3) = pts
    d1 = (x - x2) * (y1 - y2) - (x1 - x2) * (y - y2)
    d2 = (x - x3) * (y2 - y3) - (x2 - x3) * (y - y3)
    d3 = (x - x1) * (y3 - y1) - (x3 - x1) * (y - y1)
    neg = d1 < 0 or d2 < 0 or d3 < 0
    pos = d1 > 0 or d2 > 0 or d3 > 0
    return not (neg and pos)


def near_segment(x, y, a, b, width):
    (ax, ay), (bx, by) = a, b
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)))
    px, py = ax + t * dx, ay + t * dy
    return (x - px) ** 2 + (y - py) ** 2 <= (width / 2) ** 2


def sample(u, v):
    """Colour (r, g, b, a) at unit coordinates u, v in [0, 1)."""
    # A margin like the system icons have.
    margin = 0.06
    u, v = (u - margin) / (1 - 2 * margin), (v - margin) / (1 - 2 * margin)
    if not (0 <= u < 1 and 0 <= v < 1) or not inside_rounded(u, v, 1.0, 0.22):
        return (0, 0, 0, 0)
    white = (255, 255, 255, 255)
    play = ((0.40, 0.33), (0.40, 0.67), (0.68, 0.50))
    if inside_triangle(u, v, play):
        return white
    w = 0.07
    chevrons = [
        ((0.30, 0.13), (0.50, 0.24)), ((0.50, 0.24), (0.70, 0.13)),
        ((0.30, 0.87), (0.50, 0.76)), ((0.50, 0.76), (0.70, 0.87)),
    ]
    if any(near_segment(u, v, a, b, w) for a, b in chevrons):
        return (255, 255, 255, 235)
    # Diagonal gradient #2f81f7 -> #0b4fb3.
    t = (u + v) / 2
    top, bottom = (0x2F, 0x81, 0xF7), (0x0B, 0x4F, 0xB3)
    return tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)) + (255,)


def render(size):
    rows = []
    n = size * SS
    for py in range(size):
        row = bytearray([0])  # filter: none
        for px in range(size):
            acc = [0, 0, 0, 0]
            for sy in range(SS):
                for sx in range(SS):
                    r, g, b, a = sample((px * SS + sx + 0.5) / n, (py * SS + sy + 0.5) / n)
                    acc[0] += r * a
                    acc[1] += g * a
                    acc[2] += b * a
                    acc[3] += a
            alpha = acc[3] / (SS * SS)
            if acc[3]:
                row += bytes(round(c / acc[3]) for c in acc[:3]) + bytes([round(alpha)])
            else:
                row += bytes(4)
        rows.append(bytes(row))
    return png(size, b"".join(rows))


def png(size, raw):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # RGBA
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def main():
    small, large = render(64), render(256)
    images = ROOT / "app" / "ui" / "images"
    images.mkdir(parents=True, exist_ok=True)
    for path, data in [
        (ROOT / "ICON.PNG", small), (ROOT / "ICON_256.PNG", large),
        (images / "icon_64.png", small), (images / "icon_256.png", large),
    ]:
        path.write_bytes(data)
        print(path, len(data))


if __name__ == "__main__":
    main()

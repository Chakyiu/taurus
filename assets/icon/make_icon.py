"""Draws the Taurus app icon: a waveform turning into two caption lines,
the spoken line and its translation.

    python3 assets/icon/make_icon.py

writes icon.svg, icon-1024.png and Taurus.icns next to this file.
"""

import os
import shutil
import subprocess

from PIL import Image, ImageDraw, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
S = 1024
BODY = (100, 100, 924, 924)
RADIUS = 186
TOP, BOTTOM = (0x18, 0x23, 0x2C), (0x0A, 0x0E, 0x12)
ACCENT = (0x35, 0xC8, 0xD8)
INK = (0xE8, 0xEE, 0xF2)

BAR_W, BAR_GAP = 44, 30
BARS = [96, 176, 268, 340, 236, 156, 84]
WAVE_Y = 392
LEFT = (S - (len(BARS) * BAR_W + (len(BARS) - 1) * BAR_GAP)) // 2
RIGHT = S - LEFT
LINES = [(640, RIGHT, INK, 1.0), (716, LEFT + 352, ACCENT, 0.62)]
LINE_H = 44


def bars():
    for i, h in enumerate(BARS):
        x = LEFT + i * (BAR_W + BAR_GAP)
        yield (x, WAVE_Y - h // 2, x + BAR_W, WAVE_Y + h // 2)


def svg():
    hexc = lambda c: "#%02X%02X%02X" % c
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {S} {S}">',
        "<defs>",
        f'<linearGradient id="bg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{hexc(TOP)}"/>'
        f'<stop offset="1" stop-color="{hexc(BOTTOM)}"/></linearGradient>',
        '<filter id="shadow" x="-20%" y="-20%" width="140%" height="140%">'
        '<feGaussianBlur stdDeviation="22"/></filter>',
        "</defs>",
        f'<rect x="{BODY[0]}" y="{BODY[1] + 14}" width="{BODY[2] - BODY[0]}" height="{BODY[3] - BODY[1]}" '
        f'rx="{RADIUS}" fill="#000" opacity="0.4" filter="url(#shadow)"/>',
        f'<rect x="{BODY[0]}" y="{BODY[1]}" width="{BODY[2] - BODY[0]}" height="{BODY[3] - BODY[1]}" '
        f'rx="{RADIUS}" fill="url(#bg)" stroke="#FFFFFF" stroke-opacity="0.07" stroke-width="3"/>',
    ]
    for x0, y0, x1, y1 in bars():
        out.append(
            f'<rect x="{x0}" y="{y0}" width="{x1 - x0}" height="{y1 - y0}" rx="{BAR_W // 2}" fill="{hexc(ACCENT)}"/>'
        )
    for y, end, color, alpha in LINES:
        out.append(
            f'<rect x="{LEFT}" y="{y}" width="{end - LEFT}" height="{LINE_H}" rx="{LINE_H // 2}" '
            f'fill="{hexc(color)}" opacity="{alpha}"/>'
        )
    out.append("</svg>")
    return "\n".join(out) + "\n"


def png(k=4):
    big = S * k
    sc = lambda box: tuple(v * k for v in box)
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))

    shadow = Image.new("L", (big, big), 0)
    x0, y0, x1, y1 = BODY
    ImageDraw.Draw(shadow).rounded_rectangle(sc((x0, y0 + 14, x1, y1 + 14)), RADIUS * k, fill=102)
    shadow = shadow.filter(ImageFilter.GaussianBlur(22 * k))
    img.paste((0, 0, 0, 255), (0, 0), shadow)

    grad = Image.new("RGBA", (big, big))
    gd = ImageDraw.Draw(grad)
    for y in range(big):
        t = min(max((y - y0 * k) / ((y1 - y0) * k), 0), 1)
        gd.line([(0, y), (big, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,))
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle(sc(BODY), RADIUS * k, fill=255)
    img.paste(grad, (0, 0), mask)

    d = ImageDraw.Draw(img)
    d.rounded_rectangle(sc(BODY), RADIUS * k, outline=(255, 255, 255, 18), width=3 * k)
    for b in bars():
        d.rounded_rectangle(sc(b), BAR_W // 2 * k, fill=ACCENT + (255,))
    for y, end, color, alpha in LINES:
        layer = Image.new("RGBA", (big, big), (0, 0, 0, 0))
        ImageDraw.Draw(layer).rounded_rectangle(
            sc((LEFT, y, end, y + LINE_H)), LINE_H // 2 * k, fill=color + (round(255 * alpha),)
        )
        img = Image.alpha_composite(img, layer)
    return img.resize((S, S), Image.LANCZOS)


def main():
    with open(os.path.join(HERE, "icon.svg"), "w") as f:
        f.write(svg())
    icon = png()
    icon.save(os.path.join(HERE, "icon-1024.png"))
    iconset = os.path.join(HERE, "Taurus.iconset")
    os.makedirs(iconset, exist_ok=True)
    for size in (16, 32, 128, 256, 512):
        icon.resize((size, size), Image.LANCZOS).save(os.path.join(iconset, f"icon_{size}x{size}.png"))
        icon.resize((size * 2, size * 2), Image.LANCZOS).save(os.path.join(iconset, f"icon_{size}x{size}@2x.png"))
    for size in (32, 128, 256, 512):
        icon.resize((size, size), Image.LANCZOS).save(os.path.join(HERE, f"icon-{size}.png"))
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(HERE, "Taurus.icns")], check=True)
    shutil.rmtree(iconset)


if __name__ == "__main__":
    main()

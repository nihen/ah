#!/usr/bin/env python3
"""Render still images for the video: background, end card, vertical header.

Usage: video/cards.py [lang ...]
Output: video/.work/cards/<aspect>/{bg,header}.png and <lang>/end.png
"""

import json
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

VIDEO = Path(__file__).resolve().parent
WORK = VIDEO / ".work"
MONO = "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-{}.ttf"
SANS = "/usr/share/fonts/opentype/noto/NotoSansCJK-{}.ttc"  # index 0 = JP

BG = (21, 22, 30)
FG = (248, 248, 242)
MUTED = (160, 164, 190)
PURPLE = (189, 147, 249)
PINK = (255, 121, 198)
ORANGE = (255, 184, 108)
CHIP = (40, 42, 54)


def mono(weight: str, size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(MONO.format(weight), size)


def sans(weight: str, size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(SANS.format(weight), size, index=0)


def background(w: int, h: int) -> Image.Image:
    img = Image.new("RGB", (w, h), BG)
    glow = Image.new("RGB", (w, h), BG)
    d = ImageDraw.Draw(glow)
    r = int(max(w, h) * 0.35)
    d.ellipse((-r // 2, -r // 2, r, r), fill=(62, 44, 96))
    d.ellipse((w - r, h - r, w + r // 2, h + r // 2), fill=(84, 36, 70))
    glow = glow.filter(ImageFilter.GaussianBlur(r // 2))
    img = Image.blend(img, glow, 0.85)
    d = ImageDraw.Draw(img)
    step = 48
    for y in range(step // 2, h, step):
        for x in range(step // 2, w, step):
            d.point((x, y), fill=(44, 46, 60))
    return img


def gradient_text(img: Image.Image, xy: tuple[int, int], text: str, font, anchor="mm") -> None:
    """Draw text filled with a purple→pink horizontal gradient."""
    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).text(xy, text, font=font, fill=255, anchor=anchor)
    box = mask.getbbox()
    if not box:
        return
    grad = Image.new("RGB", img.size)
    gd = ImageDraw.Draw(grad)
    x0, _, x1, _ = box
    for x in range(x0, x1 + 1):
        t = (x - x0) / max(1, x1 - x0)
        gd.line([(x, 0), (x, img.height)], fill=tuple(int(a + (b - a) * t) for a, b in zip(PURPLE, PINK)))
    img.paste(grad, (0, 0), mask)


def text_block(d: ImageDraw.ImageDraw, cx: int, y: int, text: str, font, fill, spacing: float = 1.35) -> int:
    """Draw centered multi-line text starting at y; return the y below it."""
    size = font.size
    for line in text.split("\n"):
        d.text((cx, y), line, font=font, fill=fill, anchor="mt")
        y += int(size * spacing)
    return y


def code_box(d: ImageDraw.ImageDraw, cx: int, y: int, cmd: str, font) -> int:
    w = d.textlength("$ " + cmd, font=font) + font.size * 2
    h = int(font.size * 2.2)
    x = cx - w / 2
    d.rounded_rectangle((x, y, x + w, y + h), radius=16, fill=CHIP, outline=(68, 71, 90), width=2)
    tx = x + font.size
    d.text((tx, y + h / 2), "$ ", font=font, fill=PINK, anchor="lm")
    d.text((tx + d.textlength("$ ", font=font), y + h / 2), cmd, font=font, fill=FG, anchor="lm")
    return y + h


def end_card(w: int, h: int, script: dict) -> Image.Image:
    img = background(w, h)
    d = ImageDraw.Draw(img)
    s = min(w, h) / 1080 * (1.1 if h > w else 1)
    card = script["cards"]["end"]
    y = int(h * (0.24 if h > w else 0.17))
    big = mono("ExtraBold", int(170 * s))
    gradient_text(img, (w // 2, y), "ah", big, anchor="mt")
    y += int(170 * s)
    y = text_block(d, w // 2, y, card["name"], sans("Bold", int(56 * s)), FG)
    y += int(14 * s)
    y = text_block(d, w // 2, y, card["tagline"], sans("Regular", int(36 * s)), MUTED)
    y += int(40 * s)
    code = mono("Medium", int(36 * s))
    y = code_box(d, w // 2, y, "brew install nihen/tap/ah", code) + int(20 * s)
    y = code_box(d, w // 2, y, "cargo install ah-cli", code) + int(40 * s)
    d.text((w // 2, y), "github.com/nihen/ah", font=mono("Bold", int(40 * s)), fill=ORANGE, anchor="mt")
    return img


def header(w: int, y: int) -> Image.Image:
    """Transparent wordmark strip shown above the terminal in the vertical layout."""
    img = Image.new("RGBA", (w, y), (0, 0, 0, 0))
    base = Image.new("RGB", (w, y), (0, 0, 0))
    gradient_text(base, (w // 2 - 150, y // 2), "ah", mono("ExtraBold", 120))
    d = ImageDraw.Draw(base)
    d.text((w // 2 - 60, y // 2), "Agent History", font=sans("Bold", 54), fill=FG, anchor="lm")
    alpha = base.convert("L").point(lambda v: 255 if v > 8 else 0)
    img.paste(base, (0, 0), alpha)
    return img


def main() -> None:
    timeline = json.loads((VIDEO / "timeline.json").read_text())
    langs = sys.argv[1:] or ["en", "ja"]
    for aspect, a in timeline["aspects"].items():
        out = WORK / "cards" / aspect
        out.mkdir(parents=True, exist_ok=True)
        background(a["w"], a["h"]).save(out / "bg.png")
        if a["h"] > a["w"]:
            header(a["w"], a["term"]["y"]).save(out / "header.png")
        for lang in langs:
            script = json.loads((VIDEO / "script" / f"{lang}.json").read_text())
            (out / lang).mkdir(exist_ok=True)
            end_card(a["w"], a["h"], script).save(out / lang / "end.png")
            print(f"cards: {aspect}/{lang}")


if __name__ == "__main__":
    main()

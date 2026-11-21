#!/usr/bin/env python3
"""Turn the colored output of `ah stats` into a still SVG.

    ah --color stats | python3 demo/stats_svg.py > demo/stats.svg

Every character gets its own x position and the block characters are drawn as
rectangles, so the picture keeps its columns whatever font the viewer has.
"""
import re
import sys
from html import escape

CW, CH, SIZE, PAD = 9.6, 21, 16, 24
BG, FG = "#16181d", "#d6dae0"
SHADES = {"░": 0.3, "▒": 0.55, "▓": 0.8, "█": 1.0}
LEVELS = [0, 95, 135, 175, 215, 255]
BASE = ["#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5"]


def color256(n):
    if n < 16:
        return BASE[n % 8]
    if n >= 232:
        v = 8 + (n - 232) * 10
        return f"#{v:02x}{v:02x}{v:02x}"
    n -= 16
    r, g, b = LEVELS[n // 36], LEVELS[n // 6 % 6], LEVELS[n % 6]
    return f"#{r:02x}{g:02x}{b:02x}"


def cells(line):
    """(character, color, bold, dim) of every column of a line."""
    fg, bold, dim = FG, False, False
    for part in re.split(r"(\x1b\[[0-9;]*m)", line):
        m = re.fullmatch(r"\x1b\[([0-9;]*)m", part)
        if not m:
            for ch in part:
                yield ch, fg, bold, dim
            continue
        codes = [int(c) for c in m.group(1).split(";") if c] or [0]
        i = 0
        while i < len(codes):
            c = codes[i]
            if c == 0:
                fg, bold, dim = FG, False, False
            elif c == 1:
                bold = True
            elif c == 2:
                dim = True
            elif c == 22:
                bold = dim = False
            elif c == 39:
                fg = FG
            elif c == 38 and codes[i + 1 : i + 2] == [5]:
                fg = color256(codes[i + 2])
                i += 2
            i += 1


def main():
    lines = sys.stdin.read().rstrip("\n").split("\n")
    rows = [list(cells(line)) for line in lines]
    width = PAD * 2 + CW * max(len(r) for r in rows)
    height = PAD * 2 + CH * len(rows)
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width:g}" height="{height:g}" '
        f'viewBox="0 0 {width:g} {height:g}">',
        f'<rect width="100%" height="100%" rx="8" fill="{BG}"/>',
        f'<g font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,\'DejaVu Sans Mono\',monospace" '
        f'font-size="{SIZE}">',
    ]
    for y, row in enumerate(rows):
        top = PAD + y * CH
        runs = []
        for x, (ch, fg, bold, dim) in enumerate(row):
            left = PAD + x * CW
            if ch in SHADES:
                opacity = SHADES[ch] * (0.55 if dim else 1)
                out.append(
                    f'<rect x="{left:g}" y="{top + 2:g}" width="{CW - 1.2:g}" height="{CH - 4:g}" '
                    f'rx="1.5" fill="{fg}" fill-opacity="{opacity:g}"/>'
                )
            elif ch == "·" and fg != FG:
                out.append(
                    f'<rect x="{left:g}" y="{top + 2:g}" width="{CW - 1.2:g}" height="{CH - 4:g}" '
                    f'rx="1.5" fill="{fg}" fill-opacity="0.12"/>'
                )
            elif ch != " ":
                style = (fg, bold, dim)
                if runs and runs[-1][0] == style:
                    runs[-1][1].append((left, ch))
                else:
                    runs.append((style, [(left, ch)]))
        for (fg, bold, dim), chars in runs:
            xs = " ".join(f"{left:g}" for left, _ in chars)
            text = escape("".join(ch for _, ch in chars))
            attrs = f'fill="{"#ffffff" if bold else fg}"'
            if bold:
                attrs += ' font-weight="bold"'
            if dim:
                attrs += ' fill-opacity="0.55"'
            out.append(f'<text x="{xs}" y="{top + SIZE:g}" {attrs}>{text}</text>')
    out.append("</g></svg>")
    print("\n".join(out))


if __name__ == "__main__":
    main()

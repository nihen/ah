#!/usr/bin/env python3
"""Compose the final videos from recorded clips, cards, narration, and BGM.

Usage: video/compose.py [lang ...] [--aspect 16x9|9x16 ...]
Inputs: .work/clips/<aspect>/*.mp4 (record.sh), .work/cards (cards.py),
        .work/tts/<lang>/*.wav (tts.py), .work/bgm.wav (bgm.py)
Output: video/out/ah-promo-<lang>-<aspect>.mp4
"""

import json
import subprocess
import sys
from pathlib import Path

from PIL import ImageFont

VIDEO = Path(__file__).resolve().parent
WORK = VIDEO / ".work"
OUT = VIDEO / "out"
MAX_TEMPO = 1.15
SUB_FONT = "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc"


def run(cmd: list[str]) -> None:
    subprocess.run(["ffmpeg", "-v", "error", "-y", *cmd], check=True)


def duration(path: Path) -> float:
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", str(path)],
        capture_output=True, text=True, check=True,
    )
    return float(out.stdout)


def segment(tl: dict, aspect: str, lang: str, scene: dict, length: float) -> Path:
    """Render one scene as a standalone clip of exactly `length` seconds."""
    a = tl["aspects"][aspect]
    w, h, fps = a["w"], a["h"], tl["fps"]
    cards = WORK / "cards" / aspect
    seg = WORK / "seg" / aspect / lang / f"{scene['id']}.mp4"
    seg.parent.mkdir(parents=True, exist_ok=True)
    enc = ["-t", f"{length}", "-r", str(fps), "-c:v", "libx264", "-preset", "medium",
           "-crf", "16", "-pix_fmt", "yuv420p", "-an", str(seg)]
    if scene["kind"] == "card":
        frames = int(length * fps) + 1
        run(["-loop", "1", "-framerate", str(fps), "-i", str(cards / lang / f"{scene['id']}.png"),
             "-vf", f"zoompan=z='1+0.00045*on':x='iw/2-iw/zoom/2':y='ih/2-ih/zoom/2'"
                    f":d={frames}:s={w}x{h}:fps={fps}", *enc])
    else:
        t = a["term"]
        clip = WORK / "clips" / aspect / f"{scene['id']}.mp4"
        inputs = ["-loop", "1", "-i", str(cards / "bg.png"), "-i", str(clip)]
        graph = (f"[1:v]fps={fps},tpad=stop_mode=clone:stop_duration={length}[t];"
                 f"[0:v][t]overlay={t['x']}:{t['y']}:shortest=0")
        if (cards / "header.png").exists():
            inputs += ["-loop", "1", "-i", str(cards / "header.png")]
            graph += "[v];[v][2:v]overlay=0:0"
        run([*inputs, "-filter_complex", graph, *enc])
    return seg


def ass_time(t: float) -> str:
    cs = int(round(t * 100))
    return f"{cs // 360000}:{cs // 6000 % 60:02d}:{cs // 100 % 60:02d}.{cs % 100:02d}"


PUNCT = " 、。，？！—"


def wrap(text: str, font_size: int, max_w: int) -> str:
    """Fit a cue into max_w, breaking into lines only when needed.

    A "\n" in the cue marks a preferred break (libass only wraps at spaces, which
    Japanese lacks). Otherwise lines break at the space/punctuation that balances them.
    """
    width = ImageFont.truetype(SUB_FONT, font_size, index=0).getlength
    joined = text.replace("\n", "")
    if width(joined) <= max_w:
        return joined
    if "\n" in text:
        lines = [""]
        for part in text.split("\n"):
            if lines[-1] and width(lines[-1] + part) > max_w:
                lines.append("")
            lines[-1] += part
        return r"\N".join(wrap(line, font_size, max_w) for line in lines)
    best = None
    for i in range(1, len(text)):
        if text[i - 1] in PUNCT:
            left, right = text[:i].rstrip(), text[i:].lstrip()
            score = max(width(left), width(right))
            if best is None or score < best[0]:
                best = (score, left, right)
    return best[1] + r"\N" + best[2] if best else text


def subtitles(tl: dict, aspect: str, script: dict, cues: list[tuple[float, float, str]]) -> Path:
    a = tl["aspects"][aspect]
    s = a["sub"]
    path = WORK / "subs" / f"{script['lang']}-{aspect}.ass"
    path.parent.mkdir(parents=True, exist_ok=True)
    margin_lr = int(a["w"] * 0.08)
    lines = [
        "[Script Info]", "ScriptType: v4.00+", f"PlayResX: {a['w']}", f"PlayResY: {a['h']}",
        "WrapStyle: 0", "",
        "[V4+ Styles]",
        "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, "
        "Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, "
        "Shadow, Alignment, MarginL, MarginR, MarginV, Encoding",
        f"Style: Default,{script['font']},{s['size']},&H00F2F8F8,&H00F2F8F8,&H0014131A,&H6014131A,"
        f"-1,0,0,0,100,100,0,0,1,4,2,2,{margin_lr},{margin_lr},{s['margin_v']},1",
        "", "[Events]",
        "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text",
    ]
    for start, end, text in cues:
        text = wrap(text, s["size"], a["w"] - 2 * margin_lr - 2 * s["size"])
        lines.append(f"Dialogue: 0,{ass_time(start)},{ass_time(end)},Default,,0,0,0,,"
                     r"{\fad(120,120)}" + text)
    path.write_text("\n".join(lines) + "\n")
    return path


def narration(tl: dict, script: dict) -> tuple[list[tuple[Path, float, float]], list[tuple[float, float, str]]]:
    """Place each line at its scene start; speed up lines that would overrun their scene."""
    lead = tl["narration_lead"]
    t = 0.0
    placed, cues = [], []
    lines = {l["scene"]: l for l in script["lines"]}
    for scene in tl["scenes"]:
        line = lines.get(scene["id"])
        if line:
            wav = WORK / "tts" / script["lang"] / f"{scene['id']}.wav"
            dur = duration(wav)
            room = scene["duration"] - lead - 0.1
            tempo = min(MAX_TEMPO, dur / room) if dur > room else 1.0
            if dur / tempo > room + 0.05:
                print(f"  warning: {script['lang']}/{scene['id']} {dur:.2f}s overruns by "
                      f"{dur / tempo - room:.2f}s even at {tempo:.2f}x", file=sys.stderr)
            placed.append((wav, t + lead, tempo))
            cues.append((t + lead - 0.05, t + lead + dur / tempo + 0.25, line["sub"]))
        t += scene["duration"]
    return placed, cues


def compose(tl: dict, aspect: str, lang: str) -> Path:
    script = json.loads((VIDEO / "script" / f"{lang}.json").read_text())
    scenes, cf = tl["scenes"], tl["crossfade"]
    total = sum(s["duration"] for s in scenes)
    segs = [segment(tl, aspect, lang, s, s["duration"] + (cf if i < len(scenes) - 1 else 0))
            for i, s in enumerate(scenes)]
    placed, cues = narration(tl, script)
    ass = subtitles(tl, aspect, script, cues)

    inputs: list[str] = []
    for seg in segs:
        inputs += ["-i", str(seg)]
    graph, prev, offset = [], "0:v", 0.0
    for i in range(1, len(segs)):
        offset += scenes[i - 1]["duration"]
        graph.append(f"[{prev}][{i}:v]xfade=transition=fade:duration={cf}:offset={offset}[x{i}]")
        prev = f"x{i}"
    fontsdir = "/usr/share/fonts/opentype/noto"
    graph.append(f"[{prev}]ass={ass}:fontsdir={fontsdir},format=yuv420p[vout]")

    base = len(segs)
    for wav, _, _ in placed:
        inputs += ["-i", str(wav)]
    inputs += ["-i", str(WORK / "bgm.wav")]
    vox = []
    for j, (_, start, tempo) in enumerate(placed):
        ms = int(start * 1000)
        graph.append(f"[{base + j}:a]atempo={tempo},aresample=48000,aformat=channel_layouts=stereo,"
                     f"adelay={ms}|{ms}[n{j}]")
        vox.append(f"[n{j}]")
    graph.append(f"{''.join(vox)}amix=inputs={len(vox)}:normalize=0,apad=whole_dur={total},"
                 "asplit[vox][key]")
    graph.append(f"[{base + len(placed)}:a]volume=0.55[music]")
    graph.append("[music][key]sidechaincompress=threshold=0.03:ratio=6:attack=20:release=350[duck]")
    graph.append(f"[vox][duck]amix=inputs=2:normalize=0,loudnorm=I=-14:TP=-1.5:LRA=11,"
                 f"atrim=end={total}[aout]")

    OUT.mkdir(exist_ok=True)
    out = OUT / f"ah-promo-{lang}-{aspect}.mp4"
    run([*inputs, "-filter_complex", ";".join(graph), "-map", "[vout]", "-map", "[aout]",
         "-t", str(total), "-r", str(tl["fps"]), "-c:v", "libx264", "-preset", "slow", "-crf", "18",
         "-profile:v", "high", "-c:a", "aac", "-b:a", "192k", "-ar", "48000",
         "-movflags", "+faststart", str(out)])
    return out


def main() -> None:
    args = sys.argv[1:]
    tl = json.loads((VIDEO / "timeline.json").read_text())
    aspects = [args[i + 1] for i, a in enumerate(args) if a == "--aspect"] or list(tl["aspects"])
    skip = {args[i + 1] for i, a in enumerate(args) if a == "--aspect"}
    langs = [a for a in args if not a.startswith("--") and a not in skip] or ["en", "ja"]
    for aspect in aspects:
        for lang in langs:
            out = compose(tl, aspect, lang)
            print(f"{out.relative_to(VIDEO.parent)}: {duration(out):.2f}s")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Generate the background music with Lyria.

Usage: video/bgm.py [--force]
Output: video/.work/bgm.raw.mp3 (cached; --force regenerates)
compose.py calls fit() to cut it to each video's length.
"""

import subprocess
import sys
from pathlib import Path

from gemini import interact

MODEL = "lyria-3.5"
PROMPT = (
    "A 70-second instrumental track. Minimal modern electronic, calm and focused, 90 BPM, "
    "soft synth pads, clean muted plucks, light hi-hats, gentle sub bass, a subtle arpeggio "
    "joins in the second half, steady energy with no big build, ends softly. "
    "Instrumental only, no vocals."
)
VIDEO = Path(__file__).resolve().parent
WORK = VIDEO / ".work"
RAW = WORK / "bgm.raw.mp3"


def duration(path: Path) -> float:
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", str(path)],
        capture_output=True, text=True, check=True,
    )
    return float(out.stdout)


def generate(force: bool = False) -> Path:
    WORK.mkdir(exist_ok=True)
    if force or not RAW.exists():
        audio, _ = interact({"model": MODEL, "input": PROMPT})
        RAW.write_bytes(audio)
    return RAW


def fit(total: float, out: Path) -> Path:
    """Cut the track to `total` seconds with a fade-in and fade-out.

    A track shorter than the video repeats a middle section with a crossfade
    so its natural ending is kept.
    """
    clip = duration(RAW)
    fade_out = 3.0
    if clip >= total:
        graph = f"[0:a]atrim=end={total},afade=t=in:d=0.8"
    else:
        xf = 2.0
        jump = total - clip + xf
        cut = clip * 0.6
        graph = (f"[0:a]atrim=end={cut}[a];[1:a]atrim=start={cut - jump},asetpts=PTS-STARTPTS[b];"
                 f"[a][b]acrossfade=d={xf}:c1=qsin:c2=qsin,atrim=end={total}")
    graph += f",afade=t=out:st={total - fade_out}:d={fade_out}[out]"
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-i", str(RAW), "-i", str(RAW), "-filter_complex", graph,
         "-map", "[out]", "-ar", "48000", "-ac", "2", str(out)],
        check=True,
    )
    return out


if __name__ == "__main__":
    generate("--force" in sys.argv)
    print(f"bgm: {MODEL} {duration(RAW):.2f}s -> {RAW.relative_to(VIDEO.parent)}")

#!/usr/bin/env python3
"""Generate the background music with Lyria and fit it to the video length.

Usage: video/bgm.py [--force]
Output: video/.work/bgm.wav (exactly the timeline length, faded out)
The raw Lyria clip is cached in video/.work/bgm.raw.mp3; --force regenerates.
If the clip is shorter than the video, its tail is looped with a crossfade.
"""

import json
import subprocess
import sys
from pathlib import Path

from gemini import interact

MODEL = "lyria-3-clip-preview"
PROMPT = (
    "Minimal modern electronic tech product intro, 110 BPM, clean synth plucks, "
    "soft sub bass, light hi-hats, subtle build in the second half, bright and "
    "confident mood, ends with a clean resolving hit. Leaves space for a voice-over. "
    "Instrumental only, no vocals."
)
VIDEO = Path(__file__).resolve().parent
WORK = VIDEO / ".work"


def duration(path: Path) -> float:
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", str(path)],
        capture_output=True, text=True, check=True,
    )
    return float(out.stdout)


def main() -> None:
    WORK.mkdir(exist_ok=True)
    raw = WORK / "bgm.raw.mp3"
    if "--force" in sys.argv or not raw.exists():
        audio, _ = interact({"model": MODEL, "input": PROMPT})
        raw.write_bytes(audio)

    timeline = json.loads((VIDEO / "timeline.json").read_text())
    total = sum(s["duration"] for s in timeline["scenes"])
    clip = duration(raw)
    fade = 2.0
    if clip >= total:
        graph = f"[0:a]atrim=end={total},afade=t=out:st={total - fade}:d={fade}[out]"
    else:
        # Repeat a middle section: play [0, cut], then crossfade into [cut - jump, end]
        # so the clip's natural ending is kept.
        xf = 2.0
        jump = total - clip + xf
        cut = clip * 0.6
        graph = (
            f"[0:a]atrim=end={cut}[a];"
            f"[1:a]atrim=start={cut - jump},asetpts=PTS-STARTPTS[b];"
            f"[a][b]acrossfade=d={xf}:c1=qsin:c2=qsin,atrim=end={total},"
            f"afade=t=out:st={total - fade}:d={fade}[out]"
        )
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-i", str(raw), "-i", str(raw), "-filter_complex", graph,
         "-map", "[out]", "-ar", "48000", "-ac", "2", str(WORK / "bgm.wav")],
        check=True,
    )
    print(f"bgm: raw {clip:.2f}s -> {duration(WORK / 'bgm.wav'):.2f}s")


if __name__ == "__main__":
    main()

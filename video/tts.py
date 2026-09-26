#!/usr/bin/env python3
"""Generate narration lines with Gemini 3.8 Flash TTS.

Usage: video/tts.py [lang ...] [--force]
Output: video/.work/tts/<lang>/<scene>.wav (silence-trimmed, 48 kHz mono)
Results are cached by (model, voice, style, text); --force regenerates.
"""

import hashlib
import json
import subprocess
import sys
from pathlib import Path

from gemini import interact

MODEL = "gemini-3.8-flash-tts"
VIDEO = Path(__file__).resolve().parent
WORK = VIDEO / ".work"


def duration(path: Path) -> float:
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", str(path)],
        capture_output=True, text=True, check=True,
    )
    return float(out.stdout)


def trim(src: Path, dst: Path) -> None:
    # Strip leading/trailing silence so lines can be placed precisely on the timeline.
    sil = "silenceremove=start_periods=1:start_threshold=-45dB:start_silence=0.05"
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-i", str(src),
         "-af", f"{sil},areverse,{sil},areverse", "-ar", "48000", "-ac", "1", str(dst)],
        check=True,
    )


def generate(lang: str, force: bool) -> None:
    script = json.loads((VIDEO / "script" / f"{lang}.json").read_text())
    outdir = WORK / "tts" / lang
    outdir.mkdir(parents=True, exist_ok=True)
    for line in script["lines"]:
        style = line.get("style", script["style"])
        voice = line.get("voice", script["voice"])
        key = hashlib.sha256(json.dumps([MODEL, voice, style, line["tts"]]).encode()).hexdigest()[:16]
        raw = outdir / f"{line['scene']}.{key}.raw.wav"
        out = outdir / f"{line['scene']}.wav"
        if force or not raw.exists():
            audio, _ = interact({
                "model": MODEL,
                "input": [{
                    "type": "user_input",
                    "content": [{
                        "type": "text",
                        "text": line["tts"],
                        "annotations": [{"type": "speech_metadata", "style": style}],
                    }],
                }],
                "response_format": {"type": "audio"},
                "generation_config": {"speech_config": [{"voice": voice}]},
            })
            raw.write_bytes(audio)
        trim(raw, out)
        print(f"{lang}/{line['scene']}: {duration(out):.2f}s  {line['sub']}")


if __name__ == "__main__":
    args = sys.argv[1:]
    force = "--force" in args
    langs = [a for a in args if not a.startswith("--")] or ["en", "ja"]
    for lang in langs:
        generate(lang, force)

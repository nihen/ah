# Promo Video

About one-minute intro video for `ah`, in English and Japanese, for 16:9 (YouTube, web) and 9:16 (X, Shorts).

It is a single first-person debugging story told in a real terminal, with a cold open and no title card: checkout in `acme-shop` fails with "too many connections", `ah log` shows it is not in this project, `ah log -a -q` finds last week's Codex fix in `api-gateway`, `ah show` reads it, `ah resume` jumps back into it, then "no config, no daemon, no index" and an end card.

- Terminal footage: recorded with [VHS](https://github.com/charmbracelet/vhs) on a story-driven sandbox from `sandbox.py` (shared by both languages)
- Narration: Gemini 3.8 Flash TTS (`gemini-3.8-flash-tts`)
- BGM: Lyria 3.5 (`lyria-3.5`)
- Compositing: ffmpeg (hard cuts between terminal scenes, burned-in subtitles, BGM ducking under the narration, loudness normalized to -14 LUFS)

## Prerequisites

`vhs` (with `ttyd`), `ffmpeg` (with libass), `fzf` 0.53+ (the transcript preview uses `{r1}`), Python 3 with `pillow`, and the JetBrains Mono and Noto Sans CJK fonts.

Gemini API access: set `GEMINI_API_KEY`, or provide the key through an environment-managed credential that injects the `x-goog-api-key` header for `generativelanguage.googleapis.com`.

## Build

```bash
make video
```

Output: `video/out/ah-promo-{en,ja}-{16x9,9x16}.mp4`

Steps can be run individually (from `video/`):

| Step | Command | Output |
|------|---------|--------|
| Sandbox data | `python3 sandbox.py` | `/home/dev` (or `$AH_VIDEO_HOME`) |
| Terminal clips | `./record.sh [16x9 9x16] [-- scene ...]` | `.work/clips/<aspect>/<scene>.mp4` |
| Cards | `python3 cards.py [lang ...]` | `.work/cards/` |
| Narration | `python3 tts.py [lang ...] [--force]` | `.work/tts/<lang>/<scene>.wav` |
| BGM | `python3 bgm.py [--force]` | `.work/bgm.raw.mp3` |
| Compose | `python3 compose.py [lang ...] [--aspect 16x9]` | `out/` |

TTS and BGM results are cached, so re-running only calls the API when text, voice, or style changes (or with `--force`).

## Editing

- Narration and subtitles: `script/<lang>.json` (`tts` is spoken; `sub` is a list of subtitle cues timed across the narration by length, and `\n` marks a preferred line break when wrapping is needed)
- Voice and delivery: `voice` and `style` in `script/<lang>.json`
- Scene order: `timeline.json`. Each scene lasts as long as its clip or narration needs (plus `min`), so the total follows the script
- Terminal actions: `scenes/<scene>.tape`; session data: `sandbox.py`
- Adding a language: add `script/<lang>.json`; the terminal footage is reused

`bin/claude` (linked as `codex`, `gemini`, …) stands in for the agent CLI during recording: it prints the exact resume command `ah resume` launched and the tail of that session, without needing an authenticated agent.

`ah` takes a session's start time from the file's birth time, which cannot be set, so "today" sessions get modification times just after generation and their Date range reads naturally.

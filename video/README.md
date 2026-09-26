# Promo Video

30-second intro video for `ah`, in English and Japanese, for 16:9 (YouTube, web) and 9:16 (X, Shorts).

- Terminal footage: recorded with [VHS](https://github.com/charmbracelet/vhs) on the `demo/` sandbox data (shared by both languages)
- Narration: Gemini 3.8 Flash TTS (`gemini-3.8-flash-tts`)
- BGM: Lyria (`lyria-3-clip-preview`)
- Compositing: ffmpeg (crossfades, burned-in subtitles, BGM ducking under the narration, loudness normalized to -14 LUFS)

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
| Sandbox data | `./setup.sh` | `/tmp/ah-demo` |
| Terminal clips | `./record.sh [16x9 9x16] [-- scene ...]` | `.work/clips/<aspect>/<scene>.mp4` |
| Cards | `python3 cards.py [lang ...]` | `.work/cards/` |
| Narration | `python3 tts.py [lang ...] [--force]` | `.work/tts/<lang>/<scene>.wav` |
| BGM | `python3 bgm.py [--force]` | `.work/bgm.wav` |
| Compose | `python3 compose.py [lang ...] [--aspect 16x9]` | `out/` |

TTS and BGM results are cached, so re-running only calls the API when text, voice, or style changes (or with `--force`).

## Editing

- Narration and subtitles: `script/<lang>.json` (`tts` is spoken, `sub` is shown; `\n` in `sub` marks a preferred line break when wrapping is needed)
- Voice and delivery: `voice` and `style` in `script/<lang>.json`
- Scene order and length: `timeline.json` (total should stay at 30s)
- Terminal actions: `scenes/<scene>.tape`
- Adding a language: add `script/<lang>.json`; the terminal footage is reused

`bin/claude` stands in for the agent CLI during recording, so `ah resume` shows the exact resume command it launched without needing an authenticated agent.

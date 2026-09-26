"""Minimal Gemini Interactions API client shared by tts.py and bgm.py.

Sends GEMINI_API_KEY as x-goog-api-key when set; otherwise sends no key so an
environment-injected credential (e.g. a proxy-managed API credential) applies.
"""

import base64
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

ENDPOINT = "https://generativelanguage.googleapis.com/v1beta/interactions"


def interact(body: dict, timeout: int = 300) -> tuple[bytes, str]:
    """POST an interaction and return (audio bytes, mime type) of the last audio output."""
    headers = {"Content-Type": "application/json"}
    if key := os.environ.get("GEMINI_API_KEY"):
        headers["x-goog-api-key"] = key
    req = urllib.request.Request(ENDPOINT, json.dumps(body).encode(), headers)
    for attempt in range(6):
        try:
            with urllib.request.urlopen(req, timeout=timeout) as res:
                data = json.load(res)
            break
        except urllib.error.HTTPError as e:
            msg = e.read().decode()[:500]
            if e.code != 429 or attempt == 5:
                raise RuntimeError(f"{body['model']}: HTTP {e.code}: {msg}") from None
            # Rate limited: wait as long as the API asks (default 30s).
            wait = int(m.group(1)) + 2 if (m := re.search(r"retry in (\d+)s", msg)) else 30
            print(f"  rate limited, retrying in {wait}s", file=sys.stderr)
            time.sleep(wait)
    audio = [
        c
        for step in data.get("steps", [])
        if step.get("type") == "model_output"
        for c in step.get("content", [])
        if c.get("type") == "audio"
    ]
    if not audio:
        raise RuntimeError(f"{body['model']}: no audio in response: {json.dumps(data)[:500]}")
    return base64.b64decode(audio[-1]["data"]), audio[-1].get("mime_type", "")

"""Minimal Gemini Interactions API client shared by tts.py and bgm.py.

Sends GEMINI_API_KEY as x-goog-api-key when set; otherwise sends no key so an
environment-injected credential (e.g. a proxy-managed API credential) applies.
"""

import base64
import json
import os
import urllib.error
import urllib.request

ENDPOINT = "https://generativelanguage.googleapis.com/v1beta/interactions"


def interact(body: dict, timeout: int = 300) -> tuple[bytes, str]:
    """POST an interaction and return (audio bytes, mime type) of the last audio output."""
    headers = {"Content-Type": "application/json"}
    if key := os.environ.get("GEMINI_API_KEY"):
        headers["x-goog-api-key"] = key
    req = urllib.request.Request(ENDPOINT, json.dumps(body).encode(), headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            data = json.load(res)
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{body['model']}: HTTP {e.code}: {e.read().decode()[:500]}") from None
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

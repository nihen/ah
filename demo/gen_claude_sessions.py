#!/usr/bin/env python3
"""Generate Claude Code compatible session files for demo."""

import json
import uuid
import sys
import os
from datetime import datetime, timezone

def gen_uuid():
    return str(uuid.uuid4())

def make_session(session_id, cwd, prompts, responses, title, slug, timestamp_str):
    """Generate a Claude Code compatible JSONL session."""
    lines = []
    parent_uuid = None

    def compact_json(obj):
        return json.dumps(obj, ensure_ascii=False, separators=(',', ':'))

    # System line (for ah cwd resolution)
    lines.append(compact_json({
        "type": "system",
        "cwd": cwd,
        "sessionId": session_id,
    }))

    # Conversation turns
    for prompt, response in zip(prompts, responses):
        # User message
        msg_uuid = gen_uuid()
        prompt_id = gen_uuid()
        lines.append(json.dumps({
            "parentUuid": parent_uuid,
            "isSidechain": False,
            "promptId": prompt_id,
            "type": "user",
            "message": {
                "role": "user",
                "content": prompt,
            },
            "uuid": msg_uuid,
            "timestamp": timestamp_str,
            "userType": "external",
            "entrypoint": "cli",
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.81",
            "slug": slug,
        }))
        parent_uuid = msg_uuid

        # Assistant message
        msg_uuid = gen_uuid()
        lines.append(json.dumps({
            "parentUuid": parent_uuid,
            "isSidechain": False,
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": response}],
                "model": "claude-opus-4-6",
                "type": "message",
                "id": f"msg_{gen_uuid().replace('-', '')[:24]}",
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 150, "output_tokens": 80},
            },
            "type": "assistant",
            "uuid": msg_uuid,
            "timestamp": timestamp_str,
            "userType": "external",
            "entrypoint": "cli",
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.81",
            "slug": slug,
        }))
        parent_uuid = msg_uuid

    # Custom title
    lines.append(compact_json({
        "type": "custom-title",
        "customTitle": title,
        "sessionId": session_id,
    }))

    return "\n".join(lines) + "\n"


def main():
    # Read config from env/args
    config = json.loads(sys.stdin.read())

    for session in config["sessions"]:
        content = make_session(
            session_id=session["session_id"],
            cwd=session["cwd"],
            prompts=session["prompts"],
            responses=session["responses"],
            title=session["title"],
            slug=session["title"],  # use title as slug for demo
            timestamp_str=session["timestamp"],
        )
        filepath = session["filepath"]
        os.makedirs(os.path.dirname(filepath), exist_ok=True)
        with open(filepath, "w") as f:
            f.write(content)

    print(f"Generated {len(config['sessions'])} Claude sessions", file=sys.stderr)


if __name__ == "__main__":
    main()

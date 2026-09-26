#!/usr/bin/env python3
"""Generate the story-driven session sandbox used for the promo video.

Usage: video/sandbox.py
Creates a fake home directory (default /home/dev, override with AH_VIDEO_HOME)
holding Claude Code, Codex, Gemini CLI, Copilot CLI, and Cursor sessions.

The story: today in ~/src/acme-shop checkout hits "too many connections".
A week ago the same leak was fixed with Codex in ~/src/api-gateway
(pool_timeout); the video searches for it, reads it, and resumes it.

ah derives a session's start time from the file's birth time, which cannot be
set. Sessions from "today" therefore get mtimes shortly after generation so
their Date range reads naturally; older sessions only appear in views that
show the modified time.
"""

import json
import os
import shutil
import time
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

HOME = Path(os.environ.get("AH_VIDEO_HOME", "/home/dev"))
NOW = datetime.now().replace(second=0, microsecond=0)

# (agent, project, when, title, [(prompt, response), ...])
# when: minutes after generation for "today", or (days_ago, "HH:MM")
SESSIONS = [
    # ── acme-shop ─────────────────────────────────────────────
    ("claude", "acme-shop", 95, "Checkout fails with too many connections", [
        ("checkout returns 500 under load: 'FATAL: sorry, too many clients already'",
         "The checkout worker opens a new pool per request and never releases on timeout.\n"
         "Connections pile up until Postgres hits max_connections."),
        ("haven't we fixed this before somewhere?",
         "Possibly. I don't have history from other tools or repos in this session."),
    ]),
    ("cursor", "acme-shop", 40, "Add dark mode to account settings", [
        ("add a dark mode toggle to account settings",
         "Added a ThemeProvider and a toggle in AccountSettings.tsx.\nThe choice is persisted in localStorage."),
    ]),
    ("gemini", "acme-shop", (1, "18:20"), "Review database config for checkout", [
        ("review DB config before the sale weekend",
         "Two risks: max pool size is 50 per worker, and there is no pool_timeout,\n"
         "so a slow query can hold connections indefinitely."),
    ]),
    ("copilot", "acme-shop", (2, "11:05"), "Explain the order state machine", []),
    ("codex", "acme-shop", (3, "16:30"), "Upgrade to React 19", [
        ("upgrade the storefront to React 19",
         "Upgraded react and react-dom, replaced deprecated lifecycle calls,\nand fixed 4 type errors."),
    ]),
    ("claude", "acme-shop", (4, "10:15"), "Add Stripe webhook retries", [
        ("stripe webhooks sometimes fail silently, add retries",
         "Added idempotent retries with exponential backoff and a dead-letter table."),
    ]),
    # ── api-gateway ───────────────────────────────────────────
    ("codex", "api-gateway", (6, "16:40"), "Fix DB connection leak under load", [
        ("postgres connections keep piling up under load until we hit max_connections",
         "Found it: connections acquired in with_retry() are never released when a request times out.\n"
         "Fix: release on the error path and set pool_timeout = 5s."),
        ("do it, and add a regression test",
         "Done.\n- src/db/pool.rs: pool_timeout = 5s, release on error path\n"
         "- tests/pool_leak.rs: 200 concurrent requests, asserts pool drains\n"
         "p99 latency is back to 42ms."),
    ]),
    ("claude", "api-gateway", (2, "14:10"), "Add rate limiting per API key", [
        ("add per-key rate limiting",
         "Added a token bucket per API key backed by Redis, 100 req/s by default."),
    ]),
    ("gemini", "api-gateway", (5, "09:45"), "Generate OpenAPI spec from handlers", [
        ("generate an OpenAPI spec from our handlers",
         "Generated openapi.yaml with 24 endpoints and wired it into CI."),
    ]),
    ("cursor", "api-gateway", (8, "13:20"), "Rename /v1/users to /v1/accounts", [
        ("rename /v1/users to /v1/accounts with a redirect",
         "Renamed routes and added 308 redirects from the old paths."),
    ]),
    # ── infra ─────────────────────────────────────────────────
    ("claude", "infra", (2, "17:55"), "Tune RDS max_connections", [
        ("what should max_connections be on the new RDS instance?",
         "Keep 400, but every service must set a pool_timeout.\n"
         "api-gateway already uses pool_timeout = 5s; acme-shop does not."),
    ]),
    ("codex", "infra", (4, "12:00"), "Terraform: add read replica", [
        ("add a read replica for reporting",
         "Added aws_db_instance.reporting_replica and output its endpoint."),
    ]),
    ("copilot", "infra", (5, "15:30"), "Fix flaky deploy job", []),
    # ── ml-ranker ─────────────────────────────────────────────
    ("claude", "ml-ranker", (1, "09:30"), "Speed up feature extraction", [
        ("feature extraction takes 40 minutes",
         "Vectorized the n-gram step and cached embeddings: now 6 minutes."),
    ]),
    ("gemini", "ml-ranker", (3, "20:10"), "Evaluate ranker on holdout set", [
        ("evaluate the new ranker on the holdout set",
         "NDCG@10 improved from 0.412 to 0.447. Report written to eval/holdout.md."),
    ]),
    ("copilot", "ml-ranker", (9, "10:40"), "Set up experiment tracking", []),
    # ── mobile ────────────────────────────────────────────────
    ("cursor", "mobile", (1, "15:00"), "Offline mode for cart", [
        ("support adding items to the cart offline",
         "Queued cart mutations in SQLite and sync them on reconnect."),
    ]),
    ("gemini", "mobile", (6, "11:25"), "Push notification opt-in screen", [
        ("design a push notification opt-in screen",
         "Added OptInScreen with a soft prompt before the OS dialog."),
    ]),
    ("claude", "mobile", (10, "16:05"), "Fix crash on Android 15 back gesture", [
        ("app crashes on the predictive back gesture on Android 15",
         "Registered an OnBackPressedCallback and removed the legacy override."),
    ]),
    # ── docs ──────────────────────────────────────────────────
    ("claude", "docs", (5, "13:40"), "Rewrite the getting-started guide", [
        ("rewrite getting started for new contributors",
         "Rewrote it as a 10-minute path: clone, run, first PR."),
    ]),
    ("copilot", "docs", (11, "09:10"), "Fix broken links", []),
]


def mtime_of(when) -> datetime:
    if isinstance(when, int):
        return NOW + timedelta(minutes=when)
    days, hm = when
    h, m = map(int, hm.split(":"))
    return (NOW - timedelta(days=days)).replace(hour=h, minute=m)


def iso(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z")


def jl(lines) -> str:
    return "".join(json.dumps(l, ensure_ascii=False, separators=(",", ":")) + "\n" for l in lines)


def write(path: Path, text: str, mtime: datetime) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    ts = mtime.timestamp()
    os.utime(path, (ts, ts))


def claude(cwd: str, title: str, turns, mt: datetime) -> None:
    sid = str(uuid.uuid4())
    lines = [{"type": "system", "cwd": cwd, "sessionId": sid}]
    parent = None
    for prompt, response in turns:
        for role, content in (("user", prompt), ("assistant", [{"type": "text", "text": response}])):
            u = str(uuid.uuid4())
            msg = {"role": role, "content": content}
            if role == "assistant":
                msg.update({"type": "message", "model": "claude", "stop_reason": "end_turn"})
            lines.append({"parentUuid": parent, "isSidechain": False, "type": role, "message": msg,
                          "uuid": u, "timestamp": iso(mt), "userType": "external",
                          "cwd": cwd, "sessionId": sid})
            parent = u
    lines.append({"type": "custom-title", "customTitle": title, "sessionId": sid})
    encoded = cwd.replace("/", "-")
    write(HOME / ".claude/projects" / encoded / f"{sid}.jsonl", jl(lines), mt)


def codex(cwd: str, title: str, turns, mt: datetime) -> None:
    sid = str(uuid.uuid4())
    lines = [{"type": "session.start", "payload": {"id": sid, "cwd": cwd}}]
    for prompt, response in turns:
        lines.append({"type": "response_item", "payload": {
            "role": "user", "content": [{"type": "input_text", "text": prompt}]}})
        lines.append({"type": "response_item", "payload": {
            "role": "assistant", "content": [{"type": "output_text", "text": response}]}})
    slug = title.lower().replace(" ", "-").replace("/", "")
    path = HOME / ".codex/sessions" / mt.strftime("%Y/%m/%d") / f"rollout-{mt:%Y-%m-%dT%H-%M}-{slug}.jsonl"
    write(path, jl(lines), mt)
    with open(HOME / ".codex/session_index.jsonl", "a") as f:
        f.write(json.dumps({"id": sid, "thread_name": title}) + "\n")


def gemini(cwd: str, title: str, turns, mt: datetime) -> None:
    sid = str(uuid.uuid4())
    project = Path(cwd).name
    base = HOME / ".gemini/tmp" / project
    base.mkdir(parents=True, exist_ok=True)
    (base / ".project_root").write_text(cwd)
    messages = []
    for prompt, response in turns:
        messages.append({"type": "user", "content": [{"text": prompt}]})
        messages.append({"type": "gemini", "content": response})
    path = base / "chats" / f"session-{mt:%Y-%m-%dT%H-%M}-{sid}.json"
    write(path, json.dumps({"sessionId": sid, "messages": messages}), mt)


def copilot(cwd: str, title: str, turns, mt: datetime) -> None:
    sid = str(uuid.uuid4())
    text = f"cwd: {cwd}\nsummary: {title}\ncreated_at: {iso(mt)}\n"
    write(HOME / ".copilot/session-state" / sid / "workspace.yaml", text, mt)


def cursor(cwd: str, title: str, turns, mt: datetime) -> None:
    sid = str(uuid.uuid4())
    lines = []
    for prompt, response in turns:
        lines.append({"role": "user", "message": [{"text": prompt}]})
        lines.append({"role": "assistant", "message": [{"text": response}]})
    encoded = cwd.replace("/", "-")
    write(HOME / ".cursor/projects" / encoded / "agent-transcripts" / f"{sid}.jsonl", jl(lines), mt)


WRITERS = {"claude": claude, "codex": codex, "gemini": gemini, "copilot": copilot, "cursor": cursor}


def main() -> None:
    if HOME.exists():
        shutil.rmtree(HOME)
    for d in (".codex", ".claude"):
        (HOME / d).mkdir(parents=True)
    projects = sorted({p for _, p, *_ in SESSIONS})
    for p in projects:
        (HOME / "src" / p).mkdir(parents=True)
    # Oldest first so the codex index and file creation order look natural.
    for agent, project, when, title, turns in sorted(SESSIONS, key=lambda s: mtime_of(s[2])):
        WRITERS[agent](str(HOME / "src" / project), title, turns, mtime_of(when))
    # The agent scene shows the one-line setup from README "For AI Agents".
    (HOME / "src/acme-shop/AGENTS.md").write_text(
        "`ah` — cross-agent session history CLI. Run `ah -h` for usage; key commands: "
        "`ah log` (list sessions), `ah show` (view transcript), "
        "`ah log -a -q \"keyword\"` (search all).\n")
    # The cold open tails this log.
    log = HOME / "src/acme-shop/log/checkout.log"
    log.parent.mkdir(parents=True)
    t = NOW - timedelta(minutes=3)
    log.write_text("".join(
        f"{(t + timedelta(seconds=i * 7)):%H:%M:%S} {line}\n" for i, line in enumerate([
            "INFO  POST /checkout 200 182ms",
            "WARN  db pool: 48/50 connections in use",
            "ERROR POST /checkout 500 5003ms",
            "ERROR FATAL: sorry, too many clients already",
            "ERROR FATAL: sorry, too many clients already",
        ])))
    print(f"sandbox: {len(SESSIONS)} sessions in {len(projects)} projects under {HOME}")


if __name__ == "__main__":
    main()

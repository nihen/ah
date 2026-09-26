#!/usr/bin/env bash
# Generate the demo sandbox (/tmp/ah-demo) for video recording.
# Reuses demo/setup.sh; on Linux, rewrites the macOS /private/tmp paths to /tmp
# before generation so file mtimes stay intact and cwd filtering matches.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$ROOT/video/.work"
mkdir -p "$WORK"
if [[ "$(uname)" == "Darwin" ]]; then
  bash "$ROOT/demo/setup.sh"
else
  sed 's#/private/tmp#/tmp#g; s#-private-tmp-#-tmp-#g' "$ROOT/demo/setup.sh" > "$WORK/setup.sh"
  cp "$ROOT/demo/gen_claude_sessions.py" "$WORK/"
  bash "$WORK/setup.sh"
fi

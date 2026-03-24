#!/usr/bin/env bash
# Generate sandbox dummy session data for demo GIF recording.
# Usage: source demo/setup.sh
#   This sets env vars and generates data under /tmp/ah-demo/.
#   Run `source demo/teardown.sh` to clean up.

# When sourced, don't exit the parent shell on error
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  set -eo pipefail
fi

DEMO_ROOT="/tmp/ah-demo"
rm -rf "$DEMO_ROOT"

# ── Environment variables ──
export CLAUDE_CONFIG_DIR="$DEMO_ROOT/.claude"
export CODEX_HOME="$DEMO_ROOT/.codex"
export GEMINI_CLI_HOME="$DEMO_ROOT/.gemini"
export COPILOT_HOME="$DEMO_ROOT/.copilot"
export CURSOR_CONFIG_DIR="$DEMO_ROOT/.cursor"

# Pre-configure Claude Code to skip first-run setup
mkdir -p "$CLAUDE_CONFIG_DIR"
cat > "$CLAUDE_CONFIG_DIR/settings.json" << 'SETTINGS'
{
  "permissions": {
    "defaultMode": "default"
  }
}
SETTINGS
# Create minimal .claude.json with auth and trust for demo projects
# NOTE: Copies auth tokens from real config for ah resume demo (Claude Code needs valid auth).
# Output is written with restrictive permissions and cleaned up by teardown.sh.
if [ -f "$HOME/.claude.json" ]; then
  old_umask="$(umask)"
  umask 077
  python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    src = json.load(f)
# Only copy auth-related fields, skip remoteControlAtStartup etc.
keep_keys = ['oauthAccount', 'oauthAccounts']
d = {k: src[k] for k in keep_keys if k in src}
d['hasCompletedOnboarding'] = True
d['numStartups'] = 100
d['effortCalloutDismissed'] = True
d['effortCalloutV2Dismissed'] = True

d['projects'] = {}
for p in ['webapp','apiserver','mlpipeline','mobileapp','infra','docs']:
    path = f'/private/tmp/ah-demo/projects/{p}'
    d['projects'][path] = {
        'allowedTools': [],
        'hasTrustDialogAccepted': True,
        'hasCompletedProjectOnboarding': True,
        'projectOnboardingSeenCount': 10,
    }
with open(sys.argv[2], 'w') as f:
    json.dump(d, f)
" "$HOME/.claude.json" "$CLAUDE_CONFIG_DIR/.claude.json"
  umask "$old_umask"
fi

# Create dummy project directories so Cursor's path decoder can resolve them
mkdir -p "$DEMO_ROOT/projects/webapp"
mkdir -p "$DEMO_ROOT/projects/apiserver"
mkdir -p "$DEMO_ROOT/projects/mlpipeline"
mkdir -p "$DEMO_ROOT/projects/mobileapp"
mkdir -p "$DEMO_ROOT/projects/infra"
mkdir -p "$DEMO_ROOT/projects/docs"

# ── Per-agent session data ──
# Format: proj_i title_i days_ago hour min turns p1 p2 p3 p4 p5 p6
# Each agent gets a different subset with different sessions

# Claude: 18 sessions (heaviest user)
CLAUDE_DATA=(
  "0 0 0 18 00 4 0 1 2 3 0 0"
  "1 1 0 9 15 3 4 5 6 0 0 0"
  "2 2 1 16 45 5 7 8 9 10 11 0"
  "3 3 1 11 22 2 12 13 0 0 0 0"
  "4 4 2 20 08 3 14 15 16 0 0 0"
  "5 5 2 8 50 4 17 18 19 0 0 0"
  "0 6 3 13 30 3 20 3 5 0 0 0"
  "1 7 4 10 12 5 6 8 10 12 14 0"
  "2 8 5 15 55 2 16 18 0 0 0 0"
  "3 9 6 17 40 4 19 0 2 4 0 0"
  "4 10 7 9 25 3 7 9 11 0 0 0"
  "5 11 8 12 18 6 13 15 17 19 1 3"
  "0 12 9 14 05 2 5 8 0 0 0 0"
  "1 13 10 11 48 3 10 14 18 0 0 0"
  "2 14 11 16 33 4 2 6 11 16 0 0"
  "0 15 12 10 20 3 3 7 12 0 0 0"
  "3 16 12 15 10 2 0 9 0 0 0 0"
  "1 17 13 9 45 4 5 11 17 19 0 0"
)

# Codex: 12 sessions
CODEX_DATA=(
  "0 0 0 15 10 3 0 2 3 0 0 0"
  "1 3 1 10 30 4 4 6 8 10 0 0"
  "2 2 2 14 20 2 7 9 0 0 0 0"
  "4 4 2 19 15 3 14 16 18 0 0 0"
  "0 6 3 11 45 5 20 5 7 11 13 0"
  "3 9 5 16 30 2 19 2 0 0 0 0"
  "5 5 6 9 20 3 17 19 0 0 0 0"
  "1 7 7 13 55 4 6 10 14 18 0 0"
  "2 10 8 10 40 3 8 12 16 0 0 0"
  "0 12 10 15 15 2 3 9 0 0 0 0"
  "4 14 11 11 30 3 15 1 7 0 0 0"
  "3 16 13 14 20 4 0 4 13 19 0 0"
)

# Gemini: 8 sessions
GEMINI_DATA=(
  "0 0 0 16 20 3 0 1 3 0 0 0"
  "2 2 1 13 30 4 7 9 11 13 0 0"
  "1 1 3 10 45 2 4 6 0 0 0 0"
  "4 4 4 15 10 3 14 16 18 0 0 0"
  "5 5 6 11 25 5 17 19 0 2 5 0"
  "3 9 8 14 50 2 19 3 0 0 0 0"
  "0 12 10 9 35 3 8 12 15 0 0 0"
  "2 14 12 16 15 4 2 6 10 16 0 0"
)

# Copilot: 6 sessions
COPILOT_DATA=(
  "0 0 0 13 45 3 0 2 5 0 0 0"
  "1 1 2 10 20 2 4 6 0 0 0 0"
  "3 3 4 16 30 4 12 14 17 19 0 0"
  "2 8 7 11 15 3 7 10 13 0 0 0"
  "5 11 9 14 40 2 15 18 0 0 0 0"
  "4 14 12 9 55 3 1 9 16 0 0 0"
)

# Cursor: 10 sessions
CURSOR_DATA=(
  "0 0 0 17 10 4 0 1 2 3 0 0"
  "1 1 1 11 30 3 4 5 8 0 0 0"
  "2 2 2 14 55 5 7 9 10 12 15 0"
  "3 3 3 9 20 2 13 16 0 0 0 0"
  "5 5 4 16 40 3 17 18 19 0 0 0"
  "0 6 5 10 15 4 20 6 11 14 0 0"
  "4 10 6 13 45 3 7 9 15 0 0 0"
  "1 7 8 15 30 2 10 18 0 0 0 0"
  "3 13 10 11 10 3 0 4 19 0 0 0"
  "2 14 12 14 25 4 2 8 12 16 0 0"
)

PROJECTS=(
  "/private/tmp/ah-demo/projects/webapp"
  "/private/tmp/ah-demo/projects/apiserver"
  "/private/tmp/ah-demo/projects/mlpipeline"
  "/private/tmp/ah-demo/projects/mobileapp"
  "/private/tmp/ah-demo/projects/infra"
  "/private/tmp/ah-demo/projects/docs"
)

PROJECT_ENCODED=(
  "-private-tmp-ah-demo-projects-webapp"
  "-private-tmp-ah-demo-projects-apiserver"
  "-private-tmp-ah-demo-projects-mlpipeline"
  "-private-tmp-ah-demo-projects-mobileapp"
  "-private-tmp-ah-demo-projects-infra"
  "-private-tmp-ah-demo-projects-docs"
)

PROJECT_GEMINI=(
  "webapp"
  "apiserver"
  "mlpipeline"
  "mobileapp"
  "infra"
  "docs"
)

PROMPTS=(
  "implement OAuth2 authentication flow"
  "fix memory leak in worker pool"
  "add Redis caching layer for API responses"
  "refactor database migration system"
  "set up CI/CD pipeline with GitHub Actions"
  "add WebSocket support for real-time updates"
  "optimize SQL queries for the dashboard"
  "implement rate limiting middleware"
  "add unit tests for the payment module"
  "fix CORS issues in production"
  "migrate from REST to GraphQL"
  "add Docker multi-stage build"
  "implement retry logic with exponential backoff"
  "set up structured logging with tracing"
  "fix timezone handling in date parser"
  "add OpenTelemetry instrumentation"
  "refactor error handling to use thiserror"
  "implement pagination for list endpoints"
  "add health check endpoint"
  "fix race condition in session manager"
  "add gRPC streaming endpoint for metrics"
)

# Multi-line responses (\\n for JSON newlines)
RESPONSES=(
  "I'll implement that for you. Let me start by examining the current codebase.\\nLooking at the existing auth module in src/auth/mod.rs...\\nI see the current session handling. I'll extend it with the new flow."
  "Done. I've made the changes across 3 files:\\n- src/pool.rs: fixed the connection lifecycle\\n- src/config.rs: added pool_timeout setting\\n- tests/pool_test.rs: added regression test"
  "I've identified the issue. The root cause is in the connection pool configuration.\\nThe max_idle_connections was set too high, causing resource exhaustion.\\nI've adjusted the defaults and added monitoring."
  "Here's my approach:\\n1. First refactor the existing code to extract the interface\\n2. Add the new implementation behind a feature flag\\n3. Write migration tests to verify backward compatibility"
  "I've updated the configuration and added the necessary dependencies.\\nThe CI pipeline now runs lint, test, and build stages in parallel.\\nDeploy stage triggers on main branch only."
  "The tests are now passing. Here's a summary of what changed:\\n- Fixed 3 flaky tests by adding proper async handling\\n- Added 12 new test cases for edge conditions\\n- Total coverage increased from 72% to 84%"
  "I've fixed the bug. The issue was in the error handling path.\\nThe timeout error was being silently swallowed instead of propagated.\\nAdded proper error types and logging."
  "Implementation complete. I've also added integration tests.\\nThe new endpoint responds in under 5ms at p99.\\nDocumentation updated in docs/api.md."
  "I've refactored the module to use the new pattern.\\nAll call sites have been updated.\\nNo breaking changes to the public API."
  "The migration is ready. Please review the changes before applying.\\nBackup the database before running: cargo run --bin migrate\\nRollback script is in migrations/rollback_v2.sql"
)

TITLES=(
  "implement-oauth2-flow"
  "fix-memory-leak"
  "add-redis-caching"
  "refactor-db-migrations"
  "setup-cicd-pipeline"
  "add-websocket-support"
  "optimize-sql-queries"
  "implement-rate-limiting"
  "add-payment-tests"
  "fix-cors-issues"
  "migrate-to-graphql"
  "add-docker-build"
  "implement-retry-logic"
  "setup-structured-logging"
  "fix-timezone-handling"
  "add-opentelemetry"
  "refactor-error-handling"
  "implement-pagination"
  "add-health-check"
  "fix-race-condition"
)

# ── Helpers ──

gen_uuids() {
  local count=$1
  python3 -c "import uuid; [print(uuid.uuid4()) for _ in range($count)]"
}

make_ts() {
  local days_ago=$1 hour=$2 min=$3
  if [[ "$(uname)" == "Darwin" ]]; then
    date -v-"${days_ago}d" -j -f "%H:%M" "${hour}:${min}" +%s 2>/dev/null || date -v-"${days_ago}d" +%s
  else
    date -d "${days_ago} days ago ${hour}:${min}" +%s
  fi
}

touch_ts() {
  local ts=$1 file=$2
  local fmt
  if [[ "$(uname)" == "Darwin" ]]; then
    fmt=$(date -r "$ts" +%Y%m%d%H%M.%S)
  else
    fmt=$(date -d "@$ts" +%Y%m%d%H%M.%S)
  fi
  touch -t "$fmt" "$file"
}

fmt_iso() {
  if [[ "$(uname)" == "Darwin" ]]; then
    date -r "$1" +"%Y-%m-%dT%H:%M:%S"
  else
    date -d "@$1" +"%Y-%m-%dT%H:%M:%S"
  fi
}

fmt_file_dt() {
  if [[ "$(uname)" == "Darwin" ]]; then
    date -r "$1" +"%Y-%m-%dT%H-%M"
  else
    date -d "@$1" +"%Y-%m-%dT%H-%M"
  fi
}

fmt_date_path() {
  if [[ "$(uname)" == "Darwin" ]]; then
    date -r "$1" +"%Y/%m/%d"
  else
    date -d "@$1" +"%Y/%m/%d"
  fi
}

parse_session() {
  read -r S_PROJ S_TITLE S_DAYS S_HOUR S_MIN S_TURNS S_P1 S_P2 S_P3 S_P4 S_P5 S_P6 <<< "$1"
}

write_turns_claude() {
  local file=$1 turns=$2
  shift 2
  local indices=("$@")
  for t in $(seq 0 $(( turns - 1 ))); do
    local pi=${indices[$t]}
    local ri=$(( pi % ${#RESPONSES[@]} ))
    echo "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"${PROMPTS[$pi]}\"}]}}" >> "$file"
    echo "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"${RESPONSES[$ri]}\"}]}}" >> "$file"
  done
}

write_turns_codex() {
  local file=$1 turns=$2
  shift 2
  local indices=("$@")
  for t in $(seq 0 $(( turns - 1 ))); do
    local pi=${indices[$t]}
    local ri=$(( pi % ${#RESPONSES[@]} ))
    echo "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"${PROMPTS[$pi]}\"}]}}" >> "$file"
    echo "{\"type\":\"response_item\",\"payload\":{\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"${RESPONSES[$ri]}\"}]}}" >> "$file"
  done
}

write_turns_cursor() {
  local file=$1 turns=$2
  shift 2
  local indices=("$@")
  for t in $(seq 0 $(( turns - 1 ))); do
    local pi=${indices[$t]}
    local ri=$(( pi % ${#RESPONSES[@]} ))
    echo "{\"role\":\"user\",\"message\":[{\"text\":\"${PROMPTS[$pi]}\"}]}" >> "$file"
    echo "{\"role\":\"assistant\",\"message\":[{\"text\":\"${RESPONSES[$ri]}\"}]}" >> "$file"
  done
}

# ── Generate Claude sessions (18) ──
echo "Generating Claude sessions..." >&2
UUIDS=($(gen_uuids 18))

# Build JSON config for Python generator
claude_json='{"sessions":['
first=true
for i in $(seq 0 $(( ${#CLAUDE_DATA[@]} - 1 ))); do
  parse_session "${CLAUDE_DATA[$i]}"
  session_uuid="${UUIDS[$i]}"
  ts=$(make_ts "$S_DAYS" "$S_HOUR" "$S_MIN")
  timestamp=$(fmt_iso "$ts")

  proj_dir="$CLAUDE_CONFIG_DIR/projects/${PROJECT_ENCODED[$S_PROJ]}"
  filepath="$proj_dir/${session_uuid}.jsonl"

  # Collect prompts and responses for this session
  local_prompts=($S_P1 $S_P2 $S_P3 $S_P4 $S_P5 $S_P6)
  prompts_json='['
  responses_json='['
  for t in $(seq 0 $(( S_TURNS - 1 ))); do
    pi=${local_prompts[$t]}
    ri=$(( pi % ${#RESPONSES[@]} ))
    [[ "$t" -gt 0 ]] && prompts_json+=',' && responses_json+=','
    prompts_json+="\"${PROMPTS[$pi]}\""
    responses_json+="\"${RESPONSES[$ri]}\""
  done
  prompts_json+=']'
  responses_json+=']'

  $first || claude_json+=','
  first=false
  claude_json+="{\"session_id\":\"${session_uuid}\",\"cwd\":\"${PROJECTS[$S_PROJ]}\",\"filepath\":\"${filepath}\",\"title\":\"${TITLES[$S_TITLE]}\",\"timestamp\":\"${timestamp}Z\",\"prompts\":${prompts_json},\"responses\":${responses_json}}"
done
claude_json+=']}'

echo "$claude_json" | python3 "$(dirname "$0")/gen_claude_sessions.py"

# Set mtimes
for i in $(seq 0 $(( ${#CLAUDE_DATA[@]} - 1 ))); do
  parse_session "${CLAUDE_DATA[$i]}"
  session_uuid="${UUIDS[$i]}"
  ts=$(make_ts "$S_DAYS" "$S_HOUR" "$S_MIN")
  proj_dir="$CLAUDE_CONFIG_DIR/projects/${PROJECT_ENCODED[$S_PROJ]}"
  filepath="$proj_dir/${session_uuid}.jsonl"
  touch_ts "$ts" "$filepath"
done

# ── Generate Codex sessions (12) ──
echo "Generating Codex sessions..." >&2
mkdir -p "$CODEX_HOME/sessions"
index_file="$CODEX_HOME/session_index.jsonl"
> "$index_file"

UUIDS=($(gen_uuids 12))
for i in $(seq 0 $(( ${#CODEX_DATA[@]} - 1 ))); do
  parse_session "${CODEX_DATA[$i]}"
  uuid="${UUIDS[$i]}"
  ts=$(make_ts "$S_DAYS" "$S_HOUR" "$S_MIN")
  session_id="sess_${uuid//\-/}"
  session_id="${session_id:0:21}"

  dt_path=$(fmt_date_path "$ts")
  dt_file=$(fmt_file_dt "$ts")
  sess_dir="$CODEX_HOME/sessions/$dt_path"
  mkdir -p "$sess_dir"
  file="$sess_dir/rollout-${dt_file}-${TITLES[$S_TITLE]}.jsonl"

  echo "{\"type\":\"session.start\",\"payload\":{\"id\":\"${session_id}\",\"cwd\":\"${PROJECTS[$S_PROJ]}\"}}" > "$file"
  write_turns_codex "$file" "$S_TURNS" "$S_P1" "$S_P2" "$S_P3" "$S_P4" "$S_P5" "$S_P6"
  echo "{\"id\":\"${session_id}\",\"thread_name\":\"${TITLES[$S_TITLE]}\"}" >> "$index_file"
  touch_ts "$ts" "$file"
done

# ── Generate Gemini sessions (8) ──
echo "Generating Gemini sessions..." >&2

UUIDS=($(gen_uuids 8))
for i in $(seq 0 $(( ${#GEMINI_DATA[@]} - 1 ))); do
  parse_session "${GEMINI_DATA[$i]}"
  uuid="${UUIDS[$i]}"
  ts=$(make_ts "$S_DAYS" "$S_HOUR" "$S_MIN")
  dt_file=$(fmt_file_dt "$ts")

  gemini_proj="${PROJECT_GEMINI[$S_PROJ]}"
  proj_dir="$GEMINI_CLI_HOME/tmp/${gemini_proj}/chats"
  mkdir -p "$proj_dir"

  echo -n "${PROJECTS[$S_PROJ]}" > "$GEMINI_CLI_HOME/tmp/${gemini_proj}/.project_root"

  file="$proj_dir/session-${dt_file}-${uuid}.json"

  gemini_prompts=($S_P1 $S_P2 $S_P3 $S_P4 $S_P5 $S_P6)
  messages="["
  for t in $(seq 0 $(( S_TURNS - 1 ))); do
    pi=${gemini_prompts[$t]}
    ri=$(( pi % ${#RESPONSES[@]} ))
    [[ "$t" -gt 0 ]] && messages+=","
    messages+="{\"type\":\"user\",\"content\":[{\"text\":\"${PROMPTS[$pi]}\"}]}"
    messages+=",{\"type\":\"gemini\",\"content\":\"${RESPONSES[$ri]}\"}"
  done
  messages+="]"

  echo "{\"sessionId\":\"${uuid}\",\"messages\":${messages}}" > "$file"
  touch_ts "$ts" "$file"
done

# ── Generate Copilot sessions (6) ──
echo "Generating Copilot sessions..." >&2

UUIDS=($(gen_uuids 6))
for i in $(seq 0 $(( ${#COPILOT_DATA[@]} - 1 ))); do
  parse_session "${COPILOT_DATA[$i]}"
  uuid="${UUIDS[$i]}"
  ts=$(make_ts "$S_DAYS" "$S_HOUR" "$S_MIN")
  created_at=$(fmt_iso "$ts")

  sess_dir="$COPILOT_HOME/session-state/${uuid}"
  mkdir -p "$sess_dir"

  title="${TITLES[$S_TITLE]//-/ }"

  cat > "$sess_dir/workspace.yaml" <<YAML
cwd: ${PROJECTS[$S_PROJ]}
summary: ${title}
created_at: ${created_at}
YAML

  touch_ts "$ts" "$sess_dir/workspace.yaml"
done

# ── Generate Cursor sessions (10) ──
echo "Generating Cursor sessions..." >&2

UUIDS=($(gen_uuids 10))
for i in $(seq 0 $(( ${#CURSOR_DATA[@]} - 1 ))); do
  parse_session "${CURSOR_DATA[$i]}"
  uuid="${UUIDS[$i]}"
  ts=$(make_ts "$S_DAYS" "$S_HOUR" "$S_MIN")

  proj_dir="$CURSOR_CONFIG_DIR/projects/${PROJECT_ENCODED[$S_PROJ]}/agent-transcripts"
  mkdir -p "$proj_dir"
  file="$proj_dir/${uuid}.jsonl"

  > "$file"
  write_turns_cursor "$file" "$S_TURNS" "$S_P1" "$S_P2" "$S_P3" "$S_P4" "$S_P5" "$S_P6"
  touch_ts "$ts" "$file"
done

echo "" >&2
echo "Demo data generated under $DEMO_ROOT" >&2
echo "  Claude:  $(find "$CLAUDE_CONFIG_DIR" -name '*.jsonl' | wc -l | tr -d ' ') sessions" >&2
echo "  Codex:   $(find "$CODEX_HOME/sessions" -name '*.jsonl' | wc -l | tr -d ' ') sessions" >&2
echo "  Gemini:  $(find "$GEMINI_CLI_HOME" -name '*.json' | wc -l | tr -d ' ') sessions" >&2
echo "  Copilot: $(find "$COPILOT_HOME" -name 'workspace.yaml' | wc -l | tr -d ' ') sessions" >&2
echo "  Cursor:  $(find "$CURSOR_CONFIG_DIR" -name '*.jsonl' | wc -l | tr -d ' ') sessions" >&2
echo ""

# When run with bash (not sourced), print export commands for eval
# Usage: eval "$(bash demo/setup.sh)"
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  echo "export CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR"
  echo "export CODEX_HOME=$CODEX_HOME"
  echo "export GEMINI_CLI_HOME=$GEMINI_CLI_HOME"
  echo "export COPILOT_HOME=$COPILOT_HOME"
  echo "export CURSOR_CONFIG_DIR=$CURSOR_CONFIG_DIR"
  echo "export AH_COLOR=1"
fi
echo "Run 'ah log -a' to verify." >&2

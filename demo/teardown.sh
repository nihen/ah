#!/usr/bin/env bash
# Clean up demo sandbox data and unset env vars.
# Usage: source demo/teardown.sh

rm -rf /tmp/ah-demo

unset CLAUDE_CONFIG_DIR
unset CODEX_HOME
unset GEMINI_CLI_HOME
unset COPILOT_HOME
unset CURSOR_CONFIG_DIR

echo "Demo data removed and env vars unset."

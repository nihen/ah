Output demo/demo-pre.svg
Set Shell bash
Set TypingSpeed 50ms

# Sandbox env vars
Env CLAUDE_CONFIG_DIR "/tmp/ah-demo/.claude"
Env CODEX_HOME "/tmp/ah-demo/.codex"
Env GEMINI_CLI_HOME "/tmp/ah-demo/.gemini"
Env COPILOT_HOME "/tmp/ah-demo/.copilot"
Env CURSOR_CONFIG_DIR "/tmp/ah-demo/.cursor"
Env TERM "xterm-256color"
Env FORCE_COLOR "3"
Env COLORTERM "truecolor"

Sleep 1s

# Launch Claude Code once per project to initialize session state
Type "for d in webapp apiserver mlpipeline mobileapp infra docs; do (cd /tmp/ah-demo/projects/$d && echo '/exit' | claude 2>/dev/null); done"
Enter
Sleep 90s

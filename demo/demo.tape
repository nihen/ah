Output demo/demo.svg
Set Theme "Dracula"
Set Padding 10
Set Width 1200
Set Height 600
Set FontSize 20
Set Shell bash
Set TypingSpeed 46ms

# Sandbox env vars
Env CLAUDE_CONFIG_DIR "/tmp/ah-demo/.claude"
Env CODEX_HOME "/tmp/ah-demo/.codex"
Env GEMINI_CLI_HOME "/tmp/ah-demo/.gemini"
Env COPILOT_HOME "/tmp/ah-demo/.copilot"
Env CURSOR_CONFIG_DIR "/tmp/ah-demo/.cursor"
Env AH_COLOR "1"
Env AH_PAGER ""
Env TERM "xterm-256color"
Env FORCE_COLOR "3"
Env COLORTERM "truecolor"
Env FZF_DEFAULT_OPTS "--color=bg+:#44475a,fg+:#f8f8f2,hl:#bd93f9,hl+:#ff79c6,pointer:#ff79c6,marker:#50fa7b --bind=ctrl-j:preview-down,ctrl-k:preview-up"


Sleep 1150ms

# Start in a project directory (cwd filtering)
Type "cd /tmp/ah-demo/projects/webapp"
Enter
Sleep 400ms

# 1. Session listing — cwd-filtered
Type "ah log -n 3"
Sleep 1150ms
Enter
Sleep 3s

Type "clear"
Enter
Sleep 400ms

# 2. Resume latest session — Claude Code launches
Type "ah resume"
Sleep 1150ms
Enter
Sleep 6s
# Exit Claude Code
Escape
Sleep 400ms
Type "exit"
Enter
Sleep 1900ms

Type "clear"
Enter
Sleep 400ms

# 3. Cross-agent search
Type "ah log -a -q 'OAuth'"
Sleep 1150ms
Enter
Sleep 3s

Type "clear"
Enter
Sleep 400ms

# 4. Project select → session select → resume
Type "ah project -i | xargs -I{} ah resume -i -d {}"
Sleep 1150ms
Enter
Sleep 2300ms
# select project in fzf
Up
Sleep 800ms
Up
Sleep 1150ms
Enter
Sleep 2300ms
# select session in fzf
Type "claude"
Sleep 1500ms
Up
Sleep 800ms
Up
Sleep 1500ms
# Scroll transcript preview down (ctrl-j bound to preview-down)
Ctrl+J
Sleep 300ms
Ctrl+J
Sleep 300ms
Ctrl+J
Sleep 300ms
Ctrl+J
Sleep 300ms
Ctrl+J
Sleep 300ms
Ctrl+J
Sleep 300ms
Ctrl+J
Sleep 300ms
Ctrl+J
Sleep 1150ms
Enter
Sleep 6s
# Exit Claude Code
Escape
Sleep 400ms
Type "exit"
Enter
Sleep 1150ms

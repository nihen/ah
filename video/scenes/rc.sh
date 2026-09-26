# Shell setup sourced (hidden) at the start of every scene recording.
VIDEO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export HOME="${AH_VIDEO_HOME:-/home/dev}"
unset CLAUDE_CONFIG_DIR CODEX_HOME GEMINI_CLI_HOME COPILOT_HOME CURSOR_CONFIG_DIR
export PATH="$VIDEO_DIR/bin:$VIDEO_DIR/../target/release:$PATH"
export AH_COLOR=1 AH_PAGER= TERM=xterm-256color COLORTERM=truecolor
export FZF_DEFAULT_OPTS="--color=bg+:#44475a,fg+:#f8f8f2,hl:#bd93f9,hl+:#ff79c6,pointer:#ff79c6,marker:#50fa7b,prompt:#bd93f9"
export GREP_COLORS='mt=01;31'
# Two-line prompt so long commands never wrap in the narrow vertical layout.
PS1='\[\e[38;2;189;147;249m\]\w\[\e[0m\]\n\[\e[38;2;255;121;198m\]❯\[\e[0m\] '
set -o history
cd "$HOME/src/acme-shop"

# Shell setup sourced (hidden) at the start of every scene recording.
VIDEO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export HOME=/tmp/ah-demo
export CLAUDE_CONFIG_DIR="$HOME/.claude" CODEX_HOME="$HOME/.codex" GEMINI_CLI_HOME="$HOME/.gemini"
export COPILOT_HOME="$HOME/.copilot" CURSOR_CONFIG_DIR="$HOME/.cursor"
export PATH="$VIDEO_DIR/bin:$VIDEO_DIR/../target/release:$PATH"
export AH_COLOR=1 AH_PAGER= TERM=xterm-256color COLORTERM=truecolor
export FZF_DEFAULT_OPTS="--color=bg+:#44475a,fg+:#f8f8f2,hl:#bd93f9,hl+:#ff79c6,pointer:#ff79c6,marker:#50fa7b,prompt:#bd93f9"
PS1='\[\e[38;2;189;147;249m\]\w\[\e[0m\] \[\e[38;2;255;121;198m\]❯\[\e[0m\] '
cd "$HOME/projects/webapp"

# Hook scene: print the pile of session files line by line, agent dirs highlighted.
find() {
  command find "$@" 2>/dev/null | sort | sed "s#^$HOME#~#" |
    GREP_COLORS='mt=01;38;2;255;184;108' grep --color=always -E '\.(claude|codex|gemini|cursor|copilot)|$' |
    while IFS= read -r l; do printf '%s\n' "$l"; sleep 0.035; done
}

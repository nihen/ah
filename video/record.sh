#!/usr/bin/env bash
# Record terminal scenes with VHS for each aspect ratio.
# Usage: video/record.sh [aspect ...] [-- scene ...]   (defaults: all)
# Output: video/.work/clips/<aspect>/<scene>.mp4 (language-independent)
set -euo pipefail
VIDEO="$(cd "$(dirname "$0")" && pwd)"
WORK="$VIDEO/.work"
export VHS_NO_SANDBOX="${VHS_NO_SANDBOX:-true}"

aspects=(); scenes=()
while [[ $# -gt 0 && "$1" != "--" ]]; do aspects+=("$1"); shift; done
[[ "${1:-}" == "--" ]] && { shift; scenes=("$@"); }
[[ ${#aspects[@]} -eq 0 ]] && aspects=(16x9 9x16)
[[ ${#scenes[@]} -eq 0 ]] && scenes=(hook log search resume)

for aspect in "${aspects[@]}"; do
  read -r W H FONT < <(python3 -c "
import json,sys; t=json.load(open('$VIDEO/timeline.json'))['aspects']['$aspect']['term']
print(t['w'], t['h'], t['font'])")
  mkdir -p "$WORK/clips/$aspect" "$WORK/tapes/$aspect"
  for scene in "${scenes[@]}"; do
    tape="$WORK/tapes/$aspect/$scene.tape"
    cat > "$tape" <<TAPE
Output "$WORK/clips/$aspect/$scene.mp4"
Set Shell bash
Set Width $W
Set Height $H
Set FontSize $FONT
Set FontFamily "JetBrains Mono"
Set LineHeight 1.25
Set Theme "Dracula"
Set Padding 28
Set WindowBar Colorful
Set WindowBarSize 44
Set BorderRadius 14
Set Framerate 30
Set TypingSpeed 45ms
Hide
Type "source '$VIDEO/scenes/rc.sh'; clear"
Enter
Sleep 500ms
Show
TAPE
    cat "$VIDEO/scenes/$scene.tape" >> "$tape"
    echo "==> $aspect/$scene"
    vhs "$tape" >/dev/null
  done
done

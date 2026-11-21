# Demo Recording

## Prerequisites

```bash
brew install vhs
```

## Record

```bash
make demo
```

This runs: setup → pre-recording (dismiss effort dialog) → main recording.

Output: `demo/demo.svg` (embedded in README).

## Stats screen

`demo/stats.svg` is a still picture of `ah stats`, made from real output:

```bash
ah --color stats | python3 demo/stats_svg.py > demo/stats.svg
```

## Manual recording

```bash
# Generate sandbox data
bash demo/setup.sh

# Dismiss Claude Code effort dialog
vhs demo/demo-pre.tape

# Record main demo
vhs demo/demo.tape

# Clean up
source demo/teardown.sh
```

## Customizing

Edit `demo/demo.tape` to change scenarios. Edit `demo/setup.sh` to change dummy session data.

See [vhs docs](https://github.com/charmbracelet/vhs) for available commands.

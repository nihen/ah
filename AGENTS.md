# ah - Agent History

Cross-agent session search CLI written in Rust.

## Build & Install

```bash
make install                    # local build + install
```

After changing code, always run `make install` to update the binary.

## Architecture

```
src/
  main.rs        -- entry point: clap subcommand dispatch + agent/list-* commands
  cli.rs         -- clap derive CLI definition (Cli, Commands, FilterArgs, ProjectField) + help text
  config.rs      -- ~/.ahrc (TOML) parsing, agent registry (OnceLock<Vec<AgentDef>>)
  agents/        -- built-in agent plugins + registry + shared helpers
  session.rs     -- Session struct
  collector.rs   -- glob expansion + rayon parallel stat + BinaryHeap top-N
  pipeline.rs    -- unified session pipeline: collect → filter → search → resolve → sort
  projects.rs    -- project aggregation (shared by `ah project` and `ah project -i`)
  resolver.rs    -- plugin-driven metadata extraction and transcript shaping
  search.rs      -- mmap+regex full-text / plugin prompt-only search
  memory.rs      -- `ah memory` subcommand: memory + instruction file listing
  output.rs      -- TSV/LTSV/JSON output for log, project, and memory listing
  color.rs       -- color mode detection (--color/--no-color/TTY auto)
  pager.rs       -- auto-pager (less) setup for TTY output
  show.rs        -- `ah show` subcommand: pretty-print transcript + --follow
  resume.rs      -- `ah resume` subcommand: exec agent resume command
  man.rs         -- `ah man` subcommand: man page generation via clap_mangen
  fuzzy.rs       -- `-i` interactive browse via fzf/sk for log, project, memory
  subcmd.rs      -- shared session resolver (stdin pipe, session ID/path, query, filters)
```

## Key Design Decisions

- No external tools for search/parsing (all indexing and query logic in-process; no rg/jq/sed)
- Parallel stat + metadata resolution via rayon
- memmap2 for searching large JSONL files without copying
- Plugin-internal regex compiled once via LazyLock; user query regex compiled per invocation
- Single-pass filter_map for search filtering + metadata resolution
- Default cwd filtering: all subcommands filter by current directory, `-a` for global
  - Exception: `project` defaults to all (no cwd filter)
- Global options via Cli struct flatten, not per-subcommand
- log, `ah project`, and `ah memory` share the same output system (TSV/LTSV/JSON, -f field selection)
  but use separate Field enums (Field for log, ProjectField for projects, MemoryField for memory)
  Field selection via `-o/--fields` and `-O/--extra-fields`

## Configuration

- `~/.ahrc` (TOML) — optional, merge with built-in defaults
- No config = all 5 built-in agents active
- `config.rs` separates "plugins" (parse logic, static) from "agents" (config, runtime)
- `OnceLock<Vec<AgentDef>>` global registry, initialized once at startup
- Environment variables (CLAUDE_CONFIG_DIR etc.) override base directories

## Adding a New Agent

### Via ~/.ahrc (no code change)

```toml
[agents.myagent]
plugin = "claude"   # reuse existing parser (must match file format)
file_patterns = ["~/.myagent/sessions/*.jsonl"]
```

### Via code (new plugin)

1. Add a plugin under `src/agents/`
2. Register it in `src/agents/mod.rs` (PLUGINS array)
3. Implement `AgentPlugin` trait: `id`, `description`, `glob_patterns`, `path_markers`, `iter_messages`
4. Optionally implement: `resolve_cwd`, `resolve_project`, `resolve_title`, `resume_args`, `features`, `project_desc`

## Testing

```bash
cargo test
time ah -a log -n 100 > /dev/null
time ah -a -q "query" log -n 100 > /dev/null
```

## Release

1. `git switch -c release/vX.Y.Z origin/main`
2. Bump `version` in `Cargo.toml` and commit
3. Create a PR and merge

Auto-tag workflow creates tag → Release workflow builds binaries, creates GitHub Release, publishes to crates.io, updates Homebrew tap.

Branch name must start with `release/` for auto-tag to trigger.

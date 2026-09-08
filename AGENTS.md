# ah — Agent History

Cross-agent session search CLI written in Rust. Treat `README.md` as the source of truth for usage and output specifications.

## Build and verification

```bash
cargo test
make install
```

Run targeted tests during development and `cargo test` before completing changes. Run `make install` when verifying behavior with the installed CLI. If a change affects search performance, also benchmark representative `ah -a log` and `ah -a -q <query> log` commands.

## Architecture

- `cli.rs`: clap definitions
- `agents/`: built-in parser plugins and shared helpers
- `collector.rs` / `pipeline.rs` / `resolver.rs` / `search.rs`: collection, filtering, resolution, and search
- `output.rs`: shared TSV/LTSV/JSON output for log, project, and memory
- `show.rs` / `resume.rs`: transcript display and resuming sessions in the original agent

Keep search and parsing in-process. Remote aggregation uses SSH to run `ah` on the host where the data lives (`--remote` or `-A`). Ordinary subcommands filter by the current directory; `-a` selects all directories. The `project` subcommand defaults to all directories.

## Extending agents

To reuse an existing parser through configuration, use `[agents.<name>]` in `~/.ahrc`. For a new format, add a parser under `src/agents/` and register it in `src/agents/mod.rs`. Follow the existing plugins and the `AgentPlugin` trait.

## Release

Only perform release work when explicitly requested. Create a `release/vX.Y.Z` branch from the latest `origin/main`, update the version in `Cargo.toml`, and open a PR targeting `main`. After the PR is merged into `main`, the workflows create the tag and GitHub Release, publish to crates.io, and update the Homebrew tap.

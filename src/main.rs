mod agent_stats;
mod agents;
mod cancel;
mod cli;
mod collector;
mod color;
mod config;
mod fuzzy;
mod list_agents;
mod location;
mod man;
mod mcp;
mod memory;
mod output;
mod pager;
mod pipeline;
mod projects;
mod remote;
mod resolver;
mod resume;
mod search;
mod search_hits;
mod session;
mod show;
mod subcmd;

use std::process;

use clap::{CommandFactory, Parser};

use agents::common::canonical_home;
use cli::{Args, Cli, Commands, Field, ListProjectsResolvedArgs};
use color::{BOLD, DIM, RESET};

fn main() {
    // Suppress broken pipe panics (e.g. `ah log | head`)
    // SAFETY: Restoring default SIGPIPE behavior so that piping to `head` etc.
    // doesn't cause a panic on broken pipe. This is standard practice for CLI tools.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let home = canonical_home();
    config::init(&home);

    let cli = Cli::parse();

    let mut filter = cli.filter;

    // -A: expand to -a + all configured remotes (union with any explicit --remote)
    if filter.all_remote {
        filter.all = true;
        let all_names: Vec<String> = config::remotes().iter().map(|r| r.name.clone()).collect();
        if all_names.is_empty() && filter.remote.is_empty() {
            eprintln!(
                "Warning: -A specified but no remotes are configured in ~/.ahrc. \
                 Add a [remotes.<name>] section to enable remote aggregation."
            );
        }
        for name in all_names {
            if !filter.remote.contains(&name) {
                filter.remote.push(name);
            }
        }
    }
    let ia = cli.ia;
    color::init_color(filter.color, filter.no_color);
    color::init_debug(filter.debug);
    collector::init_exclude_archived(filter.no_archived);
    collector::init_include_subagents(filter.subagents);

    // --interactive-display only applies to `log -i` and `show -i`. Reject
    // it on commands that ignore it so users find out about the typo
    // immediately instead of silently getting default columns.
    if ia.interactive_display.is_some()
        && !matches!(&cli.command, Commands::Log(_) | Commands::Show(_))
    {
        eprintln!(
            "Error: --interactive-display is only supported for `ah log -i` and `ah show -i`."
        );
        process::exit(1);
    }
    // Validate the display field list up front so even short-circuit paths
    // like `ah log -i --list-fields` surface invalid field names instead of
    // silently ignoring them.
    // Up-front sanity check (catches typos / empty / `path` / etc. before
    // short-circuit paths like `--list-fields`). Pick the same
    // `allow_matched` setting the dispatched command would use, so the early
    // path doesn't silently accept invalid combinations: log = false (its
    // picker uses display-only resolve opts), show = true (query-aware).
    let allow_matched_here = matches!(&cli.command, Commands::Show(_));
    if let Err(e) = ia.parse_display_fields(allow_matched_here) {
        eprintln!("{}", e);
        process::exit(1);
    }

    // Set up pager before command dispatch (must be after color init).
    // The _pager guard keeps the pager child alive; it's waited on drop.
    let _pager = if !filter.no_pager && !ia.interactive {
        match &cli.command {
            Commands::Log(args) if args.wants_pager() => pager::setup(false),
            Commands::Search(args) if !args.json && !args.tsv => pager::setup(false),
            Commands::Show(args) if args.wants_pager() => pager::setup(false),
            Commands::Project(args) if args.wants_pager() => pager::setup(false),
            Commands::Memory(args) if args.wants_pager() => pager::setup(false),
            Commands::Agent(args) if args.wants_pager() => pager::setup(false),
            Commands::ListAgents(args) if args.wants_pager() => pager::setup(false),
            _ => None,
        }
    } else {
        None
    };

    match cli.command {
        Commands::Search(args) => {
            if let Err(e) = search_hits::run(args, filter, &ia) {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        Commands::Log(search_args) => {
            if search_args.field_list {
                print_field_list(
                    Field::all()
                        .iter()
                        .map(|f| (f.name(), f.description(), f.example())),
                    search_args.json,
                );
                return;
            }
            if ia.interactive {
                if let Err(e) = fuzzy::run_log(&search_args, &ia, &filter) {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            } else {
                let args = match Args::from_search_args(search_args, &filter) {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("{}", e);
                        process::exit(1);
                    }
                };
                if let Err(e) = run_search(args, &filter) {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            }
        }
        Commands::Show(args) => {
            if ia.interactive {
                if let Err(e) = fuzzy::run_show(&args, &ia, &filter) {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            } else if let Err(e) = show::run(args, &filter) {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        Commands::Resume(args) => {
            if ia.interactive {
                if let Err(e) = fuzzy::run_resume(&args, &ia, &filter) {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            } else if let Err(e) = resume::run(args, &filter) {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        Commands::Completion(args) => {
            let mut cmd = Cli::command();
            clap_complete::generate(args.shell, &mut cmd, "ah", &mut std::io::stdout());
        }
        Commands::Man(args) => {
            if let Some(out_dir) = args.out_dir {
                man::generate_all(&out_dir).expect("Failed to generate man pages");
            } else {
                man::generate(&mut std::io::stdout(), args.subcommand.as_deref())
                    .expect("Failed to generate man page");
            }
        }
        Commands::Project(args) => {
            if args.field_list {
                print_field_list(
                    cli::ProjectField::all()
                        .iter()
                        .map(|f| (f.name(), f.description(), f.example())),
                    args.json,
                );
                return;
            }
            if ia.interactive {
                if let Err(e) = fuzzy::run_project(&args, &ia, &filter) {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            } else {
                let resolved = match ListProjectsResolvedArgs::from_args(args) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("{}", e);
                        process::exit(1);
                    }
                };
                if let Err(e) = run_list_projects(resolved, &filter) {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            }
        }
        Commands::Memory(args) => {
            if args.field_list {
                print_field_list(
                    cli::MemoryField::all()
                        .iter()
                        .map(|f| (f.name(), f.description(), f.example())),
                    args.json,
                );
                return;
            }
            if ia.interactive {
                if let Err(e) = fuzzy::run_memory(&args, &ia, &filter) {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            } else {
                let resolved = match cli::MemoryResolvedArgs::from_args(args) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("{}", e);
                        process::exit(1);
                    }
                };
                if let Err(e) = memory::run(resolved, &filter) {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            }
        }
        Commands::Agent(agent_args) => {
            if let Err(e) = agent_stats::run(&filter, agent_args.output_format()) {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        Commands::ListAgents(args) => {
            list_agents::run(args.output_format());
        }
    }
}

fn print_field_list<'a>(fields: impl Iterator<Item = (&'a str, &'a str, &'a str)>, json: bool) {
    if json {
        let arr: Vec<_> = fields
            .map(|(name, desc, example)| {
                serde_json::json!({
                    "name": name,
                    "description": desc,
                    "example": example,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&arr).unwrap());
    } else {
        let is_tty = color::use_color();
        for (name, desc, example) in fields {
            if is_tty {
                println!(
                    "{}{:<20}{} {}{:<48}{} eg: {}",
                    BOLD, name, RESET, DIM, desc, RESET, example
                );
            } else {
                println!("{:<20} {:<48} eg: {}", name, desc, example);
            }
        }
    }
}

fn run_search(args: Args, filter: &cli::FilterArgs) -> Result<(), String> {
    let params = pipeline::PipelineParams {
        resolve_fields: args.fields.clone(),
        resolve_opts: resolver::ResolveOpts::new(
            &args.query,
            args.transcript_limit,
            args.title_limit,
        )
        .with_search_mode(args.search_mode.clone()),
        filters: args.filters.clone(),
        since: args.since,
        until: args.until,
        query: args.query.clone(),
        search_mode: args.search_mode.clone(),
        sort_field: args.sort_field,
        sort_order: args.sort_order.clone(),
        collect_limit: args.limit,
        running: args.running,
        require_resume_cmd: false,
    };
    let result = pipeline::run_pipeline(&params)?;
    let mut sessions = result.sessions;

    let mut remote_fields = args.fields.clone();
    if !remote_fields.contains(&args.sort_field) {
        remote_fields.push(args.sort_field);
    }
    remote::merge_into_sessions(
        &mut sessions,
        filter,
        &remote_fields,
        args.sort_field,
        args.sort_order.clone(),
    )?;

    if sessions.is_empty() {
        return Err("No sessions found.".to_string());
    }
    output::output_sessions(&sessions, &args.fields, &args.output_format, &args.query);
    Ok(())
}

fn run_list_projects(
    args: ListProjectsResolvedArgs,
    filter: &cli::FilterArgs,
) -> Result<(), String> {
    let mut records = match projects::build_project_records(&args, filter) {
        Ok(r) => r,
        Err(e) if !filter.remote.is_empty() && is_empty_result_error(&e) => {
            if color::is_debug() {
                eprintln!("[debug] local projects: {}", e);
            }
            Vec::new()
        }
        Err(e) => return Err(e),
    };

    // Merge remote projects if --remote is specified
    if !filter.remote.is_empty() {
        let remotes = remote::resolve_remotes(&filter.remote)?;
        let mut remote_fields = args.fields.clone();
        if !remote_fields.contains(&args.sort_field) {
            remote_fields.push(args.sort_field);
        }
        let remote_records = remote::fetch_remote_projects(&remotes, &remote_fields, filter);
        records.extend(remote_records);

        // Re-sort after merging
        let sf = args.sort_field;
        let numeric = sf.is_numeric();
        match args.sort_order {
            cli::SortOrder::Desc => records
                .sort_by(|a, b| output::compare_field_values(b.get(&sf), a.get(&sf), numeric)),
            cli::SortOrder::Asc => records
                .sort_by(|a, b| output::compare_field_values(a.get(&sf), b.get(&sf), numeric)),
        }
    }

    if records.is_empty() {
        return Err("No projects found.".to_string());
    }

    output::output_projects(&records, &args.fields, &args.output_format);
    Ok(())
}

/// Check if an error message indicates an empty result (no data found)
/// vs a real error (invalid input, parse failure, etc.).
fn is_empty_result_error(msg: &str) -> bool {
    msg.starts_with("No ") && msg.ends_with(" found.")
}

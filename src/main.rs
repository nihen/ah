mod agents;
mod cli;
mod collector;
mod color;
mod config;
mod fuzzy;
mod man;
mod memory;
mod output;
mod pager;
mod pipeline;
mod projects;
mod remote;
mod resolver;
mod resume;
mod search;
mod session;
mod show;
mod subcmd;

use std::fs;
use std::process;

use clap::{CommandFactory, Parser};

use agents::common::{canonical_home, format_mtime};
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
        for name in all_names {
            if !filter.remote.contains(&name) {
                filter.remote.push(name);
            }
        }
    }
    let ia = cli.ia;
    color::init_color(filter.color, filter.no_color);
    color::init_debug(filter.debug);

    // Set up pager before command dispatch (must be after color init).
    // The _pager guard keeps the pager child alive; it's waited on drop.
    let _pager = if !filter.no_pager && !ia.interactive {
        match &cli.command {
            Commands::Log(args) if args.wants_pager() => pager::setup(false),
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
            if let Err(e) = run_status(&filter, agent_args.output_format()) {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        Commands::ListAgents(args) => {
            let all = config::agents();
            let fmt = args.output_format();

            fn caps(p: &dyn agents::AgentPlugin) -> Vec<&'static str> {
                let mut v = Vec::new();
                if p.can_search() {
                    v.push("search");
                }
                if p.can_show() {
                    v.push("show");
                }
                if p.can_resume() {
                    v.push("resume");
                }
                if p.can_detect_running() {
                    v.push("running");
                }
                if p.can_memory() {
                    v.push("memory");
                }
                v
            }

            match fmt {
                cli::OutputFormat::Json => {
                    for agent in all {
                        let obj = serde_json::json!({
                            "id": agent.id,
                            "description": agent.description,
                            "capabilities": caps(agent.plugin),
                            "patterns": agent.glob_patterns,
                            "project_desc": agent.plugin.project_desc(),
                        });
                        println!("{}", obj);
                    }
                }
                cli::OutputFormat::Tsv => {
                    for agent in all {
                        println!(
                            "{}\t{}\t{}\t{}",
                            agent.id,
                            agent.description,
                            caps(agent.plugin).join(","),
                            agent.glob_patterns.join(","),
                        );
                    }
                }
                cli::OutputFormat::Ltsv => {
                    for agent in all {
                        println!(
                            "id:{}\tdescription:{}\tcapabilities:{}\tpatterns:{}",
                            agent.id,
                            agent.description,
                            caps(agent.plugin).join(","),
                            agent.glob_patterns.join(","),
                        );
                    }
                }
                cli::OutputFormat::Log | cli::OutputFormat::Table => {
                    let builtins: Vec<_> = all.iter().filter(|a| a.is_builtin).collect();
                    let custom: Vec<_> = all.iter().filter(|a| !a.is_builtin).collect();

                    println!("Built-in agents:");
                    println!();
                    for agent in &builtins {
                        let status = if agent.disabled { " (disabled)" } else { "" };
                        println!("{:<12} {}{}", agent.id, agent.description, status);
                        println!(
                            "             Patterns:     {}",
                            agent.glob_patterns.join(", ")
                        );
                        println!(
                            "             Capabilities: {}",
                            caps(agent.plugin).join(", ")
                        );
                        println!("             Project:      {}", agent.plugin.project_desc());
                        println!();
                    }

                    if !custom.is_empty() {
                        println!("Custom agents (~/.ahrc):");
                        println!();
                        for agent in &custom {
                            let status = if agent.disabled { " (disabled)" } else { "" };
                            println!("{:<12} plugin: {}{}", agent.id, agent.plugin.id(), status);
                            println!(
                                "             Patterns:     {}",
                                agent.glob_patterns.join(", ")
                            );
                            println!(
                                "             Capabilities: {}",
                                caps(agent.plugin).join(", ")
                            );
                            println!();
                        }
                    }
                }
            }
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

    // Merge remote sessions if --remote is specified
    if !filter.remote.is_empty() {
        let remotes = remote::resolve_remotes(&filter.remote)?;
        let mut remote_fields = args.fields.clone();
        if !remote_fields.contains(&args.sort_field) {
            remote_fields.push(args.sort_field);
        }
        let remote_sessions = remote::fetch_remote_sessions(&remotes, &remote_fields, filter);
        sessions.extend(remote_sessions);

        // Re-sort after merging
        let numeric = args.sort_field.is_numeric();
        match args.sort_order {
            cli::SortOrder::Desc => sessions.sort_by(|a, b| {
                output::compare_field_values(
                    b.fields.get(&args.sort_field),
                    a.fields.get(&args.sort_field),
                    numeric,
                )
            }),
            cli::SortOrder::Asc => sessions.sort_by(|a, b| {
                output::compare_field_values(
                    a.fields.get(&args.sort_field),
                    b.fields.get(&args.sort_field),
                    numeric,
                )
            }),
        }

        // Re-apply limit after merge
        if args.limit > 0 && sessions.len() > args.limit {
            sessions.truncate(args.limit);
        }
    }

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

fn run_status(filter: &cli::FilterArgs, output_format: cli::OutputFormat) -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::time::SystemTime;

    let home = canonical_home();
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| fs::canonicalize(&p).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let since = filter.since_time()?;
    let until = filter.until_time()?;

    let files = collector::collect_files(filter.limit);
    if files.is_empty() && filter.remote.is_empty() {
        return Err("No session files found.".to_string());
    }

    struct AgentStats {
        total: usize,
        latest: SystemTime,
        cwd_count: usize,
        cwd_latest: Option<SystemTime>,
    }

    let resolve_fields = [Field::Cwd];
    let mut stats: BTreeMap<&str, AgentStats> = BTreeMap::new();

    for (path, mtime) in &files {
        // Apply time filters
        if let Some(ref since) = since {
            if mtime < since {
                continue;
            }
        }
        if let Some(ref until) = until {
            if mtime > until {
                continue;
            }
        }

        let agent = config::find_agent_for_path(path);
        let plugin = agent
            .map(|a| a.plugin)
            .unwrap_or_else(agents::unknown_plugin);
        let id = agent.map(|a| a.id.as_str()).unwrap_or("unknown");

        // Apply --agent filter
        if let Some(ref agent_filter) = filter.agent {
            if id != agent_filter {
                continue;
            }
        }

        let fields = resolver::resolve_fields(
            path,
            plugin,
            *mtime,
            &home,
            &resolve_fields,
            &Default::default(),
        );
        let session_cwd = fields.get(&Field::Cwd).map(|v| v.as_str()).unwrap_or("");
        let in_cwd = session_cwd == cwd;

        let entry = stats.entry(id).or_insert(AgentStats {
            total: 0,
            latest: SystemTime::UNIX_EPOCH,
            cwd_count: 0,
            cwd_latest: None,
        });
        entry.total += 1;
        if *mtime > entry.latest {
            entry.latest = *mtime;
        }
        if in_cwd {
            entry.cwd_count += 1;
            if entry.cwd_latest.is_none_or(|t| *mtime > t) {
                entry.cwd_latest = Some(*mtime);
            }
        }
    }

    // Collect rows: (agent, count, latest)
    let mut rows: Vec<(String, usize, String)> = if filter.all {
        stats
            .iter()
            .map(|(agent, s)| (agent.to_string(), s.total, format_mtime(s.latest)))
            .collect()
    } else {
        let cwd_rows: Vec<_> = stats
            .iter()
            .filter(|(_, s)| s.cwd_count > 0)
            .map(|(agent, s)| {
                (
                    agent.to_string(),
                    s.cwd_count,
                    format_mtime(s.cwd_latest.unwrap()),
                )
            })
            .collect();
        if cwd_rows.is_empty() && filter.remote.is_empty() {
            return Err("No sessions in current directory.".to_string());
        }
        cwd_rows
    };

    // Merge remote agent stats if --remote is specified
    if !filter.remote.is_empty() {
        let remotes = remote::resolve_remotes(&filter.remote)?;
        let remote_stats = remote::fetch_remote_agent_stats(&remotes, filter);
        for (remote_name, rs) in remote_stats {
            let label = format!("{} ({})", rs.agent, remote_name);
            rows.push((label, rs.sessions, rs.latest));
        }
    }

    if rows.is_empty() {
        return Err("No sessions found.".to_string());
    }

    match output_format {
        cli::OutputFormat::Json => {
            for (agent, count, latest) in &rows {
                let obj = serde_json::json!({
                    "agent": agent,
                    "sessions": count,
                    "latest": latest,
                });
                println!("{}", obj);
            }
        }
        cli::OutputFormat::Ltsv => {
            for (agent, count, latest) in &rows {
                println!("agent:{}\tsessions:{}\tlatest:{}", agent, count, latest);
            }
        }
        cli::OutputFormat::Log | cli::OutputFormat::Table => {
            let is_tty = color::use_color();
            let (b, d, r) = if is_tty {
                (BOLD, DIM, RESET)
            } else {
                ("", "", "")
            };
            // Compute column widths
            let agent_w = rows
                .iter()
                .map(|(a, _, _)| a.len())
                .max()
                .unwrap_or(5)
                .max(5);
            let count_w = rows
                .iter()
                .map(|(_, c, _)| c.to_string().len())
                .max()
                .unwrap_or(8)
                .max(8);
            // Header
            println!(
                "{d}{:<agent_w$}  {:>count_w$}  LATEST{r}",
                "AGENT", "SESSIONS"
            );
            for (agent, count, latest) in &rows {
                println!(
                    "{b}{:<agent_w$}{r}  {:>count_w$}  {d}{}{r}",
                    agent, count, latest
                );
            }
        }
        cli::OutputFormat::Tsv => {
            for (agent, count, latest) in &rows {
                println!("{}\t{}\t{}", agent, count, latest);
            }
        }
    }
    Ok(())
}

/// Build a map of sessionId → PID for currently running sessions.
/// Currently Claude only (reads ~/.claude/sessions/<pid>.json).
pub fn build_pid_map() -> std::collections::HashMap<String, u32> {
    let mut map = std::collections::HashMap::new();

    let home = agents::common::canonical_home();
    let claude_base = config::resolve_agent_base("claude").unwrap_or_else(|| home.join(".claude"));
    let sessions_dir = claude_base.join("sessions");

    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                        let pid = val.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let session_id =
                            val.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
                        if pid > 0 && is_pid_alive(pid) && !session_id.is_empty() {
                            map.insert(session_id.to_string(), pid);
                        }
                    }
                }
            }
        }
    }

    map
}

#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    match i32::try_from(pid) {
        Ok(p) => unsafe { libc::kill(p, 0) == 0 },
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub fn is_pid_alive(_pid: u32) -> bool {
    false
}

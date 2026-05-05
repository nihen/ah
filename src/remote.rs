use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process;

use rayon::prelude::*;

use crate::cli::{Field, FilterArgs, MemoryField, ProjectField, SortOrder};
use crate::color;
use crate::config::{self, RemoteDef};
use crate::output::compare_field_values;
use crate::resolver::shell_quote;
use crate::session::Session;

/// Build the SSH command arguments to run `ah log --json` on a remote host.
fn build_remote_args(remote: &RemoteDef, fields: &[Field], filter: &FilterArgs) -> Vec<String> {
    let mut args = vec![
        remote.ah_path.clone(),
        "log".to_string(),
        "--json".to_string(),
    ];

    // Remote always runs with -a (no local cwd filtering makes sense)
    args.push("-a".to_string());

    let mut field_names: Vec<String> = fields.iter().map(|f| f.name().to_string()).collect();
    // Always include fields required for session identification and sorting
    for required in ["path", "modified_at", "running"] {
        if !field_names.iter().any(|f| f == required) {
            field_names.push(required.to_string());
        }
    }
    args.push("-o".to_string());
    args.push(field_names.join(","));
    if let Some(ref q) = filter.query {
        args.push("-q".to_string());
        args.push(q.clone());
    }
    if filter.prompt_only {
        args.push("-p".to_string());
    }
    if let Some(ref a) = filter.agent {
        args.push("--agent".to_string());
        args.push(a.clone());
    }
    if let Some(ref p) = filter.project {
        args.push("--project".to_string());
        args.push(p.clone());
    }
    if filter.limit > 0 {
        args.push("-n".to_string());
        args.push(filter.limit.to_string());
    }
    if let Some(ref s) = filter.since {
        args.push("--since".to_string());
        args.push(s.clone());
    }
    if let Some(ref u) = filter.until {
        args.push("--until".to_string());
        args.push(u.clone());
    }
    if filter.running {
        args.push("--running".to_string());
    }

    args
}

/// Parse JSON lines from remote `ah log --json` output into Sessions.
fn parse_remote_sessions(remote_name: &str, stdout: &str) -> Result<Vec<Session>, String> {
    let mut sessions = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("Parse error from remote '{}': {}", remote_name, e))?;
        let obj = val
            .as_object()
            .ok_or_else(|| format!("Expected JSON object from remote '{}'", remote_name))?;

        let mut fields = BTreeMap::new();
        for (key, value) in obj {
            if let Ok(field) = key.parse::<Field>() {
                let v = match value {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    _ => value.to_string(),
                };
                fields.insert(field, v);
            }
        }

        // Skip records that have no path. `build_remote_args` always
        // requests `path` from the remote, so a missing/empty value means
        // the record is unusable — tagging it as `<remote>:` would produce a
        // non-unique reference that breaks later show/resume routing.
        let remote_path = match fields.get(&Field::Path) {
            Some(p) if !p.is_empty() => p.clone(),
            _ => {
                eprintln!(
                    "Warning: skipping remote '{}' record with missing/empty `path` field",
                    remote_name
                );
                continue;
            }
        };
        let tagged_path = format!("{}:{}", remote_name, remote_path);
        fields.insert(Field::Path, tagged_path.clone());

        sessions.push(Session {
            path: PathBuf::from(tagged_path),
            fields,
        });
    }
    Ok(sessions)
}

/// Fetch sessions from a single remote host via SSH.
fn fetch_one(
    remote: &RemoteDef,
    fields: &[Field],
    filter: &FilterArgs,
) -> Result<Vec<Session>, String> {
    let ah_args = build_remote_args(remote, fields, filter);
    let stdout = run_ssh_capture(&remote.name, &remote.host, &ah_args)?;
    parse_remote_sessions(&remote.name, &stdout)
}

/// Resolve remote names to RemoteDefs, validating they exist in config.
pub fn resolve_remotes(names: &[String]) -> Result<Vec<&'static RemoteDef>, String> {
    let mut remotes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let remote = config::find_remote(name).ok_or_else(|| {
            let available: Vec<&str> = config::remotes().iter().map(|r| r.name.as_str()).collect();
            if available.is_empty() {
                format!(
                    "Unknown remote '{}'. No remotes configured in ~/.ahrc.\n\
                     Add a [remotes.{}] section:\n\n\
                     [remotes.{}]\n\
                     host = \"hostname\"",
                    name, name, name
                )
            } else {
                format!(
                    "Unknown remote '{}'. Available: {}",
                    name,
                    available.join(", ")
                )
            }
        })?;
        remotes.push(remote);
    }
    Ok(remotes)
}

/// Fetch remote sessions, append to `local`, then re-sort and re-apply `filter.limit`.
/// Sessions returned from the remote pipeline are tagged with `remote_name:` in
/// their `Field::Path`; the merged list is otherwise structurally identical.
pub fn merge_into_sessions(
    local: &mut Vec<Session>,
    filter: &FilterArgs,
    fields: &[Field],
    sort_field: Field,
    sort_order: SortOrder,
) -> Result<(), String> {
    if filter.remote.is_empty() {
        return Ok(());
    }
    let remotes = resolve_remotes(&filter.remote)?;
    let remote_sessions = fetch_remote_sessions(&remotes, fields, filter);
    local.extend(remote_sessions);

    let numeric = sort_field.is_numeric();
    let cmp = |a: &Session, b: &Session| {
        compare_field_values(
            a.fields.get(&sort_field),
            b.fields.get(&sort_field),
            numeric,
        )
    };
    match sort_order {
        SortOrder::Desc => local.sort_by(|a, b| cmp(b, a)),
        SortOrder::Asc => local.sort_by(cmp),
    }
    if filter.limit > 0 && local.len() > filter.limit {
        local.truncate(filter.limit);
    }
    Ok(())
}

/// Fetch sessions from multiple remotes in parallel.
pub fn fetch_remote_sessions(
    remotes: &[&RemoteDef],
    fields: &[Field],
    filter: &FilterArgs,
) -> Vec<Session> {
    if remotes.is_empty() {
        return Vec::new();
    }

    let debug = color::is_debug();
    if debug {
        let names: Vec<&str> = remotes.iter().map(|r| r.name.as_str()).collect();
        eprintln!(
            "[debug] fetching from {} remote(s): {}",
            remotes.len(),
            names.join(", ")
        );
    }

    let results: Vec<Result<Vec<Session>, String>> = remotes
        .par_iter()
        .map(|remote| fetch_one(remote, fields, filter))
        .collect();

    let mut all_sessions = Vec::new();
    for result in results {
        match result {
            Ok(sessions) => all_sessions.extend(sessions),
            Err(e) => eprintln!("Warning: {}", e),
        }
    }

    all_sessions
}

// ---------------------------------------------------------------------------
// Remote path parsing: detect "remotename:path" format
// ---------------------------------------------------------------------------

/// Parse a remote path reference like "mydev:/home/user/.claude/projects/abc.jsonl".
/// Returns (remote_name, remote_path) if the prefix matches a configured remote.
pub fn parse_remote_path(s: &str) -> Option<(&'static RemoteDef, &str)> {
    let colon = s.find(':')?;
    let name = &s[..colon];
    let path = &s[colon + 1..];
    if path.is_empty() {
        return None;
    }
    config::find_remote(name).map(|r| (r, path))
}

/// Check if a string looks like a remote reference (contains `:`) but the remote
/// name is not configured. Returns an error message if so.
pub fn check_unknown_remote(s: &str) -> Result<(), String> {
    let configured: Vec<&str> = config::remotes().iter().map(|r| r.name.as_str()).collect();
    check_unknown_remote_with(s, &configured)
}

fn check_unknown_remote_with(s: &str, configured: &[&str]) -> Result<(), String> {
    let Some(colon) = s.find(':') else {
        return Ok(());
    };
    let name = &s[..colon];
    let path = &s[colon + 1..];
    if name.is_empty()
        || path.is_empty()
        || configured.contains(&name)
        || name.contains('/')
        || name.contains('\\')
        || looks_like_windows_drive(name, path)
    {
        return Ok(());
    }
    // Any unknown `name:value` is treated as a typo'd remote ref, regardless
    // of whether remotes are currently configured. Both `typo:/abs/path` and
    // `typo:SESSION_ID` therefore surface a clear error instead of silently
    // failing later as "No session found for id".
    //
    // Trade-off: a legitimate local file containing a colon (e.g.
    // `a:b.jsonl`) will be flagged here. Workaround: prefix with `./`
    // (`./a:b.jsonl`) — the name component then contains `/` and is
    // short-circuited above.
    Err(if configured.is_empty() {
        format!(
            "Unknown remote '{}'. No remotes configured in ~/.ahrc.",
            name
        )
    } else {
        format!(
            "Unknown remote '{}'. Available: {}",
            name,
            configured.join(", ")
        )
    })
}

fn looks_like_windows_drive(name: &str, path: &str) -> bool {
    cfg!(windows)
        && name.len() == 1
        && name.as_bytes()[0].is_ascii_alphabetic()
        && (path.starts_with('/') || path.starts_with('\\'))
}

// ---------------------------------------------------------------------------
// show: exec SSH to stream transcript from remote
// ---------------------------------------------------------------------------

/// Exec `ssh host ah show <path> [flags]` — replaces current process.
/// Metadata mode (`-o` / `--tsv`) is captured and re-tagged locally so the
/// `path` field comes back as `<remote>:/<abs>` instead of the remote-side
/// raw local path; otherwise the call falls through to a bare `exec_ssh`.
pub fn exec_remote_show(
    remote: &RemoteDef,
    remote_path: &str,
    args: &crate::cli::ShowArgs,
    filter: &crate::cli::FilterArgs,
) -> ! {
    // The remote path is forwarded verbatim. The remote side's
    // `resolve_session_ref` does raw-first / TSV-decoded-fallback, so
    // `mydev:C:\foo` (raw typed) and `mydev:C:\\foo` (TSV-escaped pipe
    // output) both resolve correctly without any local decoding here —
    // local decoding would corrupt raw refs that legitimately contain
    // `\t` / `\n` / `\r` sequences in the remote path.
    let metadata_mode = args.common.fields.is_some() || args.tsv;
    if metadata_mode {
        // `meta_fields()` is guaranteed `Some(_)` here because metadata_mode
        // is true (either `-o` was set, or `--tsv` defaults to `[Title]`).
        match args.meta_fields() {
            Ok(Some(fields)) => match run_remote_show_meta(remote, remote_path, &fields, filter) {
                Ok(()) => process::exit(0),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            },
            Ok(None) => unreachable!("metadata_mode is true => meta_fields() is Some"),
            Err(e) => {
                eprintln!("Error: {}", e);
                process::exit(1);
            }
        }
    }

    let format = args.format();
    let mut ah_args = vec![remote.ah_path.clone(), "show".to_string()];

    match format {
        crate::cli::ShowFormat::Raw => ah_args.push("--raw".to_string()),
        crate::cli::ShowFormat::Json => ah_args.push("--json".to_string()),
        crate::cli::ShowFormat::Md => ah_args.push("--md".to_string()),
        crate::cli::ShowFormat::Tsv => ah_args.push("--tsv".to_string()),
        crate::cli::ShowFormat::Pretty => {}
    }
    // Forward query/search-mode so query-dependent fields like `matched`
    // resolve on the remote against the same query the user supplied.
    if let Some(q) = filter.query.as_deref() {
        ah_args.push("-q".to_string());
        ah_args.push(q.to_string());
    }
    if filter.prompt_only {
        ah_args.push("--prompt-only".to_string());
    }
    if let Some(n) = args.head {
        ah_args.push("--head".to_string());
        ah_args.push(n.to_string());
    }
    if args.follow {
        ah_args.push("--follow".to_string());
    }
    if let Some(pattern) = args.highlight.as_deref() {
        ah_args.push(format!("--highlight={}", pattern));
    }
    if crate::color::use_color() {
        ah_args.push("--color".to_string());
    } else {
        ah_args.push("--no-color".to_string());
    }
    // Prevent the remote ah from starting its own pager
    ah_args.push("--no-pager".to_string());

    ah_args.push(remote_path.to_string());

    // Only allocate a pty for pretty/follow modes. Non-tty formats
    // (raw/json/md) would otherwise get LF→CRLF translation and a
    // "Pseudo-terminal will not be allocated" warning when stdout/stdin
    // is piped.
    let want_tty = matches!(format, crate::cli::ShowFormat::Pretty) || args.follow;
    exec_ssh(&remote.host, &ah_args, want_tty)
}

/// Run `ssh host ah show <path> -o <fields> ...` and stream the captured
/// TSV with the `path` column re-tagged as `<remote>:<remote_path>` and
/// `resume_cmd` wrapped with `ssh -t -- <host> ...` so the local consumer
/// can run it directly. Streaming line-by-line keeps memory flat for
/// unbounded fields like `transcript` / `messages` / `responses`.
pub fn run_remote_show_meta(
    remote: &RemoteDef,
    remote_path: &str,
    fields: &[Field],
    filter: &crate::cli::FilterArgs,
) -> Result<(), String> {
    let field_names: Vec<&str> = fields.iter().map(|f| f.name()).collect();
    let mut ah_args = vec![
        remote.ah_path.clone(),
        "show".to_string(),
        "-o".to_string(),
        field_names.join(","),
    ];
    if let Some(q) = filter.query.as_deref() {
        ah_args.push("-q".to_string());
        ah_args.push(q.to_string());
    }
    if filter.prompt_only {
        ah_args.push("--prompt-only".to_string());
    }
    ah_args.push("--no-color".to_string());
    ah_args.push("--no-pager".to_string());
    ah_args.push(remote_path.to_string());

    let path_idxs: Vec<usize> = fields
        .iter()
        .enumerate()
        .filter_map(|(i, f)| (*f == Field::Path).then_some(i))
        .collect();
    let resume_idxs: Vec<usize> = fields
        .iter()
        .enumerate()
        .filter_map(|(i, f)| (*f == Field::ResumeCmd).then_some(i))
        .collect();
    run_ssh_streaming(remote, &ah_args, |line| {
        let out = retag_remote_show_meta_line(line, &path_idxs, &resume_idxs, remote);
        println!("{}", out);
    })
}

/// Re-tag the `path` and `resume_cmd` columns of a single remote
/// `ah show -o ...` TSV line. `path` becomes `<remote_name>:<remote_path>`;
/// `resume_cmd` is wrapped in `ssh -t -- <host> <quoted command>`. Both are
/// decoded via `unescape_tsv` before transformation and re-escaped via
/// `escape_tsv` afterwards so embedded tabs/newlines stay TSV-safe. All
/// duplicate `path` / `resume_cmd` columns are rewritten — taking only
/// the first would silently leak an untagged remote path / unwrapped
/// resume command in the later column(s).
pub(crate) fn retag_remote_show_meta_line(
    line: &str,
    path_idxs: &[usize],
    resume_idxs: &[usize],
    remote: &RemoteDef,
) -> String {
    if path_idxs.is_empty() && resume_idxs.is_empty() {
        return line.to_string();
    }
    let mut cols: Vec<String> = line.split('\t').map(str::to_string).collect();
    for &idx in path_idxs {
        if let Some(col) = cols.get_mut(idx) {
            let decoded = crate::output::unescape_tsv(col);
            *col = crate::output::escape_tsv(&format!("{}:{}", remote.name, decoded));
        }
    }
    for &idx in resume_idxs {
        if let Some(col) = cols.get_mut(idx) {
            let decoded = crate::output::unescape_tsv(col);
            let wrapped = format!(
                "ssh -t -- {} {}",
                shell_quote(&remote.host),
                shell_quote(&decoded)
            );
            *col = crate::output::escape_tsv(&wrapped);
        }
    }
    cols.join("\t")
}

/// Spawn `ssh -o BatchMode=yes -o ConnectTimeout=10 -- <host> <cmd>` and
/// invoke `on_line` for each newline-terminated stdout chunk. Streaming
/// avoids buffering large `transcript`/`messages` payloads in memory.
/// Stderr is drained concurrently in a background thread so a verbose
/// remote can't fill its stderr pipe and block stdout progress.
fn run_ssh_streaming<F: FnMut(&str)>(
    remote: &RemoteDef,
    args: &[String],
    mut on_line: F,
) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Read};
    let remote_cmd = quote_remote_command(args);
    let debug = color::is_debug();
    if debug {
        eprintln!(
            "[debug] remote '{}': ssh {} {}",
            remote.name, remote.host, remote_cmd
        );
    }
    let mut child = process::Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("--")
        .arg(&remote.host)
        .arg(&remote_cmd)
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("SSH to remote '{}' ({}): {}", remote.name, remote.host, e))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("SSH '{}': stdout unavailable", remote.name))?;
    // Drain stderr concurrently so a verbose remote can't deadlock us by
    // filling the stderr pipe before we finish reading stdout.
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            buf
        })
    });
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("SSH '{}': stdout read: {}", remote.name, e))?;
        on_line(&line);
    }
    let stderr_buf = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let status = child
        .wait()
        .map_err(|e| format!("SSH '{}': wait: {}", remote.name, e))?;
    if !status.success() {
        let trimmed = stderr_buf.trim();
        if trimmed.contains("No sessions found")
            || trimmed.contains("No projects found")
            || trimmed.contains("No memory files found")
            || trimmed.contains("No session files found")
        {
            return Ok(());
        }
        return Err(format!(
            "Remote '{}' ({}): {}",
            remote.name, remote.host, trimmed
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// resume: exec SSH to resume session on remote
// ---------------------------------------------------------------------------

/// Exec `ssh -t host ah resume <ref> [-- extra_args]` — replaces current process.
pub fn exec_remote_resume(
    remote: &RemoteDef,
    remote_ref: &str,
    args: &crate::cli::ResumeArgs,
) -> ! {
    // Forward the remote ref verbatim — see `exec_remote_show` for the
    // raw-first/decoded-fallback rationale (handled remote-side).
    let mut ah_args = vec![remote.ah_path.clone(), "resume".to_string()];

    if args.print {
        ah_args.push("--print".to_string());
    }

    ah_args.push(remote_ref.to_string());

    if !args.extra_args.is_empty() {
        ah_args.push("--".to_string());
        ah_args.extend(args.extra_args.iter().cloned());
    }

    // Resume always wants a pty: it execs the agent CLI which is interactive.
    exec_ssh(&remote.host, &ah_args, true)
}

/// Format a remote resume command for `--print` output.
pub fn format_remote_resume_command(
    remote: &RemoteDef,
    remote_ref: &str,
    extra_args: &[String],
) -> String {
    let mut parts = vec![
        "ssh".to_string(),
        "-t".to_string(),
        "--".to_string(),
        shell_quote(&remote.host),
        shell_quote(&remote.ah_path),
        "resume".to_string(),
        shell_quote(remote_ref),
    ];
    if !extra_args.is_empty() {
        parts.push("--".to_string());
        for a in extra_args {
            parts.push(shell_quote(a));
        }
    }
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// project: fetch remote project records
// ---------------------------------------------------------------------------

/// Fetch project records from remotes in parallel.
pub fn fetch_remote_projects(
    remotes: &[&RemoteDef],
    fields: &[ProjectField],
    filter: &FilterArgs,
) -> Vec<BTreeMap<ProjectField, String>> {
    if remotes.is_empty() {
        return Vec::new();
    }

    let results: Vec<Result<Vec<BTreeMap<ProjectField, String>>, String>> = remotes
        .par_iter()
        .map(|remote| fetch_one_projects(remote, fields, filter))
        .collect();

    let mut all = Vec::new();
    for result in results {
        match result {
            Ok(records) => all.extend(records),
            Err(e) => eprintln!("Warning: {}", e),
        }
    }
    all
}

fn fetch_one_projects(
    remote: &RemoteDef,
    fields: &[ProjectField],
    filter: &FilterArgs,
) -> Result<Vec<BTreeMap<ProjectField, String>>, String> {
    let mut args = vec![
        remote.ah_path.clone(),
        "project".to_string(),
        "--json".to_string(),
    ];

    let field_names: Vec<&str> = fields.iter().map(|f| f.name()).collect();
    args.push("-o".to_string());
    args.push(field_names.join(","));

    forward_common_filters(&mut args, filter);

    let stdout = run_ssh_capture(&remote.name, &remote.host, &args)?;
    parse_json_records(&remote.name, &stdout)
}

// ---------------------------------------------------------------------------
// agent: fetch remote agent stats
// ---------------------------------------------------------------------------

/// Agent stats row from remote.
pub struct RemoteAgentStats {
    pub agent: String,
    pub sessions: usize,
    pub latest: String,
}

/// Fetch agent stats from remotes in parallel.
pub fn fetch_remote_agent_stats(
    remotes: &[&RemoteDef],
    filter: &FilterArgs,
) -> Vec<(String, RemoteAgentStats)> {
    if remotes.is_empty() {
        return Vec::new();
    }

    let results: Vec<Result<Vec<(String, RemoteAgentStats)>, String>> = remotes
        .par_iter()
        .map(|remote| fetch_one_agent_stats(remote, filter))
        .collect();

    let mut all = Vec::new();
    for result in results {
        match result {
            Ok(stats) => all.extend(stats),
            Err(e) => eprintln!("Warning: {}", e),
        }
    }
    all
}

fn fetch_one_agent_stats(
    remote: &RemoteDef,
    filter: &FilterArgs,
) -> Result<Vec<(String, RemoteAgentStats)>, String> {
    let mut args = vec![
        remote.ah_path.clone(),
        "agent".to_string(),
        "--json".to_string(),
        "-a".to_string(),
    ];

    if let Some(ref a) = filter.agent {
        args.push("--agent".to_string());
        args.push(a.clone());
    }
    if filter.limit > 0 {
        args.push("-n".to_string());
        args.push(filter.limit.to_string());
    }
    if let Some(ref s) = filter.since {
        args.push("--since".to_string());
        args.push(s.clone());
    }
    if let Some(ref u) = filter.until {
        args.push("--until".to_string());
        args.push(u.clone());
    }

    let stdout = run_ssh_capture(&remote.name, &remote.host, &args)?;
    let mut stats = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            let agent = val
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let sessions = val.get("sessions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let latest = val
                .get("latest")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !agent.is_empty() {
                stats.push((
                    remote.name.clone(),
                    RemoteAgentStats {
                        agent,
                        sessions,
                        latest,
                    },
                ));
            }
        }
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// memory: fetch remote memory records
// ---------------------------------------------------------------------------

/// Fetch memory records from remotes in parallel.
pub fn fetch_remote_memory(
    remotes: &[&RemoteDef],
    fields: &[MemoryField],
    filter: &FilterArgs,
    memory_type: Option<&str>,
) -> Vec<BTreeMap<MemoryField, String>> {
    if remotes.is_empty() {
        return Vec::new();
    }

    let results: Vec<Result<Vec<BTreeMap<MemoryField, String>>, String>> = remotes
        .par_iter()
        .map(|remote| fetch_one_memory(remote, fields, filter, memory_type))
        .collect();

    let mut all = Vec::new();
    for result in results {
        match result {
            Ok(records) => all.extend(records),
            Err(e) => eprintln!("Warning: {}", e),
        }
    }
    all
}

fn fetch_one_memory(
    remote: &RemoteDef,
    fields: &[MemoryField],
    filter: &FilterArgs,
    memory_type: Option<&str>,
) -> Result<Vec<BTreeMap<MemoryField, String>>, String> {
    let mut args = vec![
        remote.ah_path.clone(),
        "memory".to_string(),
        "--json".to_string(),
        "-a".to_string(),
    ];

    let field_names: Vec<&str> = fields.iter().map(|f| f.name()).collect();
    args.push("-o".to_string());
    args.push(field_names.join(","));

    if let Some(ref a) = filter.agent {
        args.push("--agent".to_string());
        args.push(a.clone());
    }
    if let Some(ref p) = filter.project {
        args.push("--project".to_string());
        args.push(p.clone());
    }
    if let Some(ref q) = filter.query {
        args.push("-q".to_string());
        args.push(q.clone());
    }
    if let Some(t) = memory_type {
        args.push("-t".to_string());
        args.push(t.to_string());
    }
    if let Some(ref s) = filter.since {
        args.push("--since".to_string());
        args.push(s.clone());
    }
    if let Some(ref u) = filter.until {
        args.push("--until".to_string());
        args.push(u.clone());
    }

    let stdout = run_ssh_capture(&remote.name, &remote.host, &args)?;
    parse_json_records(&remote.name, &stdout)
}

// ---------------------------------------------------------------------------
// SSH execution helpers
// ---------------------------------------------------------------------------

/// Common filter forwarding for project/memory subcommands.
fn forward_common_filters(args: &mut Vec<String>, filter: &FilterArgs) {
    // Project/memory: always -a on remote (local cwd doesn't apply)
    args.push("-a".to_string());

    if let Some(ref q) = filter.query {
        args.push("-q".to_string());
        args.push(q.clone());
    }
    if let Some(ref a) = filter.agent {
        args.push("--agent".to_string());
        args.push(a.clone());
    }
    if let Some(ref p) = filter.project {
        args.push("--project".to_string());
        args.push(p.clone());
    }
    if filter.limit > 0 {
        args.push("-n".to_string());
        args.push(filter.limit.to_string());
    }
    if let Some(ref s) = filter.since {
        args.push("--since".to_string());
        args.push(s.clone());
    }
    if let Some(ref u) = filter.until {
        args.push("--until".to_string());
        args.push(u.clone());
    }
}

/// Join args into a single shell-quoted command string for SSH remote execution.
/// SSH concatenates argv with spaces and passes to the remote shell,
/// so each argument must be individually shell-quoted.
fn quote_remote_command(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run SSH command, capture stdout. Returns stdout as String.
fn run_ssh_capture(remote_name: &str, host: &str, args: &[String]) -> Result<String, String> {
    let remote_cmd = quote_remote_command(args);
    let debug = color::is_debug();
    if debug {
        eprintln!(
            "[debug] remote '{}': ssh {} {}",
            remote_name, host, remote_cmd
        );
    }

    let t0 = std::time::Instant::now();
    let output = process::Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("--")
        .arg(host)
        .arg(&remote_cmd)
        .output()
        .map_err(|e| format!("SSH to remote '{}' ({}): {}", remote_name, host, e))?;

    if debug {
        eprintln!(
            "[debug] remote '{}': SSH completed in {:.1}ms (status={})",
            remote_name,
            t0.elapsed().as_secs_f64() * 1000.0,
            output.status,
        );
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.contains("No sessions found")
            || stderr.contains("No projects found")
            || stderr.contains("No memory files found")
            || stderr.contains("No session files found")
        {
            return Ok(String::new());
        }
        return Err(format!("Remote '{}' ({}): {}", remote_name, host, stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Parse JSON lines into BTreeMap records with any FromStr key type.
fn parse_json_records<F: std::str::FromStr + Ord>(
    remote_name: &str,
    stdout: &str,
) -> Result<Vec<BTreeMap<F, String>>, String> {
    let mut records = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("Parse error from remote '{}': {}", remote_name, e))?;
        let obj = val
            .as_object()
            .ok_or_else(|| format!("Expected JSON object from remote '{}'", remote_name))?;
        let mut record = BTreeMap::new();
        for (key, value) in obj {
            if let Ok(field) = key.parse::<F>() {
                let v = match value {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    _ => value.to_string(),
                };
                record.insert(field, v);
            }
        }
        records.push(record);
    }
    Ok(records)
}

/// Exec SSH, optionally allocating a pty (`-t`). Replaces the current process.
///
/// When `want_tty` is false (non-pretty `show`), we add `BatchMode=yes` so a
/// piped stdout never hangs on a password prompt. With `want_tty=true` (pretty
/// show, follow, resume) we leave authentication interactive.
#[cfg(unix)]
fn exec_ssh(host: &str, args: &[String], want_tty: bool) -> ! {
    use std::os::unix::process::CommandExt;
    let remote_cmd = quote_remote_command(args);
    let mut cmd = process::Command::new("ssh");
    if want_tty {
        cmd.arg("-t");
    } else {
        cmd.arg("-o").arg("BatchMode=yes");
    }
    let err = cmd
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("--")
        .arg(host)
        .arg(&remote_cmd)
        .exec();
    eprintln!("Failed to exec ssh: {}", err);
    process::exit(1);
}

#[cfg(not(unix))]
fn exec_ssh(_host: &str, _args: &[String], _want_tty: bool) -> ! {
    eprintln!("Remote SSH exec is not supported on this platform");
    process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_remote() -> RemoteDef {
        RemoteDef {
            name: "mydev".to_string(),
            host: "mydev.example.com".to_string(),
            ah_path: "ah".to_string(),
        }
    }

    #[test]
    fn retag_show_meta_prefixes_path_column() {
        let remote = fake_remote();
        let line = "/srv/sessions/abc.jsonl\tcodex-001";
        let out = retag_remote_show_meta_line(line, &[0], &[], &remote);
        assert_eq!(out, "mydev:/srv/sessions/abc.jsonl\tcodex-001");
    }

    #[test]
    fn retag_show_meta_no_path_or_resume_passes_through() {
        // Neither index set: the line is forwarded verbatim.
        let remote = fake_remote();
        let line = "codex\tadd redis caching";
        assert_eq!(
            retag_remote_show_meta_line(line, &[], &[], &remote),
            "codex\tadd redis caching"
        );
    }

    #[test]
    fn retag_show_meta_decodes_then_re_encodes_path() {
        // Remote emits a path with a literal tab, `escape_tsv`'d to `\t`.
        // Re-tagging must decode, prefix, and re-encode so the row stays
        // well-formed and the path column reads as `<remote>:<original>`.
        let remote = fake_remote();
        let line = "/srv/odd\\ttab.jsonl\tcodex-001";
        let out = retag_remote_show_meta_line(line, &[0], &[], &remote);
        assert_eq!(out, "mydev:/srv/odd\\ttab.jsonl\tcodex-001");
    }

    #[test]
    fn retag_show_meta_wraps_resume_cmd_with_ssh() {
        // Remote-side resume_cmd is a shell command. Wrapping with
        // `ssh -t -- <host> <quoted command>` makes it executable from the
        // local machine via `sh`.
        let remote = fake_remote();
        let line = "abc-001\tcd '/srv/repo' && 'codex' 'resume' 'abc-001'";
        let out = retag_remote_show_meta_line(line, &[], &[1], &remote);
        // Column 0 (id) is unchanged; column 1 is wrapped.
        let cols: Vec<&str> = out.split('\t').collect();
        assert_eq!(cols[0], "abc-001");
        assert!(
            cols[1].starts_with("ssh -t -- 'mydev.example.com' "),
            "expected SSH wrap, got: {:?}",
            cols[1]
        );
        assert!(
            cols[1].contains("codex"),
            "wrapped resume_cmd should still contain the agent name: {:?}",
            cols[1]
        );
    }

    #[test]
    fn retag_show_meta_handles_duplicate_path_columns() {
        // `-o path,id,path` has two `path` columns; both must be re-tagged.
        let remote = fake_remote();
        let line = "/srv/a.jsonl\tcodex-001\t/srv/a.jsonl";
        let out = retag_remote_show_meta_line(line, &[0, 2], &[], &remote);
        assert_eq!(out, "mydev:/srv/a.jsonl\tcodex-001\tmydev:/srv/a.jsonl");
    }

    #[test]
    fn test_build_remote_args_minimal() {
        let remote = RemoteDef {
            name: "mydev".to_string(),
            host: "mydev.example.com".to_string(),
            ah_path: "ah".to_string(),
        };
        let fields = vec![Field::Agent, Field::Title, Field::ModifiedAt];
        let filter = FilterArgs {
            agent: None,
            project: None,
            dir: None,
            all: false,
            all_remote: false,
            query: None,
            prompt_only: false,
            limit: 0,
            since: None,
            until: None,
            running: false,
            remote: vec![],
            color: false,
            no_color: false,
            no_pager: false,
            debug: false,
        };
        let args = build_remote_args(&remote, &fields, &filter);
        assert_eq!(
            args,
            vec![
                "ah",
                "log",
                "--json",
                "-a",
                "-o",
                "agent,title,modified_at,path,running"
            ]
        );
    }

    #[test]
    fn test_build_remote_args_with_filters() {
        let remote = RemoteDef {
            name: "dev".to_string(),
            host: "dev".to_string(),
            ah_path: "/usr/local/bin/ah".to_string(),
        };
        let fields = vec![Field::Agent, Field::Title];
        let filter = FilterArgs {
            agent: Some("claude".to_string()),
            project: None,
            dir: None,
            all: true,
            all_remote: false,
            query: Some("auth".to_string()),
            prompt_only: false,
            limit: 10,
            since: Some("3d".to_string()),
            until: None,
            running: false,
            remote: vec![],
            color: false,
            no_color: false,
            no_pager: false,
            debug: false,
        };
        let args = build_remote_args(&remote, &fields, &filter);
        assert!(args.contains(&"-q".to_string()));
        assert!(args.contains(&"auth".to_string()));
        assert!(args.contains(&"--agent".to_string()));
        assert!(args.contains(&"claude".to_string()));
        assert!(args.contains(&"-n".to_string()));
        assert!(args.contains(&"10".to_string()));
        assert!(args.contains(&"--since".to_string()));
        assert!(args.contains(&"3d".to_string()));
    }

    #[test]
    fn test_parse_remote_sessions_empty() {
        let sessions = parse_remote_sessions("test", "").unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_parse_remote_sessions_json() {
        let json = r#"{"agent":"claude","title":"fix bug","modified_at":"2026-03-20 10:30","path":"~/.claude/projects/abc.jsonl","id":"abc123"}"#;
        let sessions = parse_remote_sessions("mydev", json).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(
            s.fields.get(&Field::Agent).map(|v| v.as_str()),
            Some("claude")
        );
        assert_eq!(
            s.fields.get(&Field::Title).map(|v| v.as_str()),
            Some("fix bug")
        );
        // Path should be tagged with remote name
        assert!(s.fields.get(&Field::Path).unwrap().starts_with("mydev:"));
    }

    #[test]
    fn test_parse_remote_sessions_multi_line() {
        let json = r#"{"agent":"claude","title":"one","modified_at":"2026-03-20 10:30","id":"1","path":"/p/a.jsonl"}
{"agent":"codex","title":"two","modified_at":"2026-03-19 09:00","id":"2","path":"/p/b.jsonl"}
"#;
        let sessions = parse_remote_sessions("dev", json).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_parse_remote_sessions_skips_records_with_missing_path() {
        // Records without a usable `path` field would be tagged as `dev:` and
        // collide with each other; skip them and warn.
        let json = r#"{"agent":"claude","id":"1"}
{"agent":"claude","id":"2","path":"/real/path.jsonl"}
{"agent":"claude","id":"3","path":""}
"#;
        let sessions = parse_remote_sessions("dev", json).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(
            sessions[0]
                .fields
                .get(&Field::Path)
                .unwrap()
                .ends_with("/real/path.jsonl")
        );
    }

    #[test]
    fn check_unknown_remote_with_no_configured_still_flags_typo() {
        // Plan requirement: "Unknown remote prefix produces clear error".
        // When no remotes are configured the message explains that.
        let err = check_unknown_remote_with("typo:/tmp/foo", &[]).unwrap_err();
        assert!(err.contains("Unknown remote 'typo'"));
        assert!(err.contains("No remotes configured"));

        let err = check_unknown_remote_with("typo:abc-123", &[]).unwrap_err();
        assert!(err.contains("Unknown remote 'typo'"));
    }

    #[test]
    fn check_unknown_remote_with_configured_flags_typo_path() {
        let err = check_unknown_remote_with("typo:/tmp/foo", &["mydev"]).unwrap_err();
        assert!(err.contains("Unknown remote 'typo'"));
        assert!(err.contains("mydev"));
    }

    #[test]
    fn check_unknown_remote_with_configured_flags_typo_session_id() {
        // Regression: `ah resume typo:SESSION_ID` must surface a typo error
        // even though the value side has no leading slash.
        let err = check_unknown_remote_with("typo:abc-123", &["mydev"]).unwrap_err();
        assert!(err.contains("Unknown remote 'typo'"));
    }

    #[test]
    fn check_unknown_remote_with_configured_passes_known_remote() {
        assert!(check_unknown_remote_with("mydev:/tmp/foo", &["mydev"]).is_ok());
        assert!(check_unknown_remote_with("mydev:abc-123", &["mydev"]).is_ok());
    }

    #[test]
    fn check_unknown_remote_with_configured_passes_when_name_has_slash() {
        // The `./` workaround for unusual local files with a colon.
        assert!(check_unknown_remote_with("./a:b.jsonl", &["mydev"]).is_ok());
        assert!(check_unknown_remote_with("/abs/path:foo", &["mydev"]).is_ok());
    }

    #[test]
    fn check_unknown_remote_with_passes_no_colon() {
        assert!(check_unknown_remote_with("abc-123", &["mydev"]).is_ok());
        assert!(check_unknown_remote_with("/path/foo.jsonl", &["mydev"]).is_ok());
    }
}

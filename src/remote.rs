use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process;

use rayon::prelude::*;

use crate::cli::{Field, FilterArgs, MemoryField, ProjectField};
use crate::color;
use crate::config::{self, RemoteDef};
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

        let remote_path = fields.get(&Field::Path).cloned().unwrap_or_default();
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
    if let Some(colon) = s.find(':') {
        let name = &s[..colon];
        let path = &s[colon + 1..];
        if !name.is_empty() && !path.is_empty() && config::find_remote(name).is_none() {
            // Only flag if the name looks like a remote (no slashes, not a Windows drive path).
            if !name.contains('/') && !name.contains('\\') && !looks_like_windows_drive(name, path)
            {
                let available: Vec<&str> =
                    config::remotes().iter().map(|r| r.name.as_str()).collect();
                return if available.is_empty() {
                    Err(format!(
                        "Unknown remote '{}'. No remotes configured in ~/.ahrc.",
                        name
                    ))
                } else {
                    Err(format!(
                        "Unknown remote '{}'. Available: {}",
                        name,
                        available.join(", ")
                    ))
                };
            }
        }
    }
    Ok(())
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
pub fn exec_remote_show(remote: &RemoteDef, remote_path: &str, args: &crate::cli::ShowArgs) -> ! {
    let mut ah_args = vec![remote.ah_path.clone(), "show".to_string()];

    // Format flags
    match args.format() {
        crate::cli::ShowFormat::Raw => ah_args.push("--raw".to_string()),
        crate::cli::ShowFormat::Json => ah_args.push("--json".to_string()),
        crate::cli::ShowFormat::Md => ah_args.push("--md".to_string()),
        crate::cli::ShowFormat::Pretty => {}
    }
    if let Some(n) = args.head {
        ah_args.push("--head".to_string());
        ah_args.push(n.to_string());
    }
    if args.follow {
        ah_args.push("--follow".to_string());
    }
    if crate::color::use_color() {
        ah_args.push("--color".to_string());
    } else {
        ah_args.push("--no-color".to_string());
    }
    // Prevent the remote ah from starting its own pager
    ah_args.push("--no-pager".to_string());

    ah_args.push(remote_path.to_string());

    exec_ssh_interactive(&remote.host, &ah_args)
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
    let mut ah_args = vec![remote.ah_path.clone(), "resume".to_string()];

    if args.print {
        ah_args.push("--print".to_string());
    }

    ah_args.push(remote_ref.to_string());

    if !args.extra_args.is_empty() {
        ah_args.push("--".to_string());
        ah_args.extend(args.extra_args.iter().cloned());
    }

    exec_ssh_interactive(&remote.host, &ah_args)
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

/// Exec SSH with -t for interactive terminal (show/resume).
#[cfg(unix)]
fn exec_ssh_interactive(host: &str, args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let remote_cmd = quote_remote_command(args);
    let err = process::Command::new("ssh")
        .arg("-t")
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
fn exec_ssh_interactive(_host: &str, _args: &[String]) -> ! {
    eprintln!("Remote SSH exec is not supported on this platform");
    process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let json = r#"{"agent":"claude","title":"one","modified_at":"2026-03-20 10:30","id":"1"}
{"agent":"codex","title":"two","modified_at":"2026-03-19 09:00","id":"2"}
"#;
        let sessions = parse_remote_sessions("dev", json).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_check_unknown_remote_rejects_single_char_remote_name() {
        let err = check_unknown_remote("x:/tmp/session.jsonl").unwrap_err();
        assert!(err.contains("Unknown remote 'x'"));
    }
}

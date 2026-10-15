//! `ah agent`: session counts per agent, overall and inside one directory.
use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::Serialize;

use crate::agents::common::{canonical_home, format_mtime};
use crate::cli::{self, Field};
use crate::color::{self, BOLD, DIM, RESET};
use crate::{agents, collector, config, remote, resolver};

/// Per-agent totals. `scope_*` count the sessions whose cwd equals the
/// directory the stats were built for.
#[derive(Debug, Clone, Serialize)]
pub struct AgentStat {
    pub agent: String,
    pub sessions: usize,
    pub latest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_sessions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_latest: Option<String>,
}

/// Count local sessions per agent (sorted by agent id). `scope_dir` is the
/// canonical directory whose sessions are counted separately; `None` skips
/// that breakdown. Agents with no sessions are not listed.
pub fn build_agent_stats(
    filter: &cli::FilterArgs,
    scope_dir: Option<&str>,
) -> Result<Vec<AgentStat>, String> {
    let home = canonical_home();
    let cwd = scope_dir.unwrap_or("");

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
        if crate::cancel::checkpoint() {
            break;
        }
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

        let in_cwd = if scope_dir.is_some() {
            let fields = resolver::resolve_fields(
                path,
                plugin,
                *mtime,
                &home,
                &resolve_fields,
                &Default::default(),
            );
            fields.get(&Field::Cwd).map(|v| v.as_str()).unwrap_or("") == cwd
        } else {
            false
        };

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

    Ok(stats
        .into_iter()
        .map(|(agent, s)| AgentStat {
            agent: agent.to_string(),
            sessions: s.total,
            latest: format_mtime(s.latest),
            scope_sessions: scope_dir.map(|_| s.cwd_count),
            scope_latest: scope_dir.map(|_| s.cwd_latest.map(format_mtime).unwrap_or_default()),
        })
        .collect())
}

/// Canonical current directory, as the CLI scopes `ah agent` by default.
pub fn current_dir_string() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// `ah agent`: print the per-agent summary (cwd rows unless `-a`).
pub fn run(filter: &cli::FilterArgs, output_format: cli::OutputFormat) -> Result<(), String> {
    let cwd = current_dir_string();
    let stats = build_agent_stats(filter, if filter.all { None } else { Some(&cwd) })?;

    // Rows are (agent, remote_name, count, latest). `remote_name` is empty
    // for local rows; structured output puts it in its own column / field so
    // consumers don't have to parse "agent (remote)" labels.
    let mut rows: Vec<(String, String, usize, String)> = if filter.all {
        stats
            .iter()
            .map(|s| (s.agent.clone(), String::new(), s.sessions, s.latest.clone()))
            .collect()
    } else {
        let cwd_rows: Vec<_> = stats
            .iter()
            .filter(|s| s.scope_sessions.unwrap_or(0) > 0)
            .map(|s| {
                (
                    s.agent.clone(),
                    String::new(),
                    s.scope_sessions.unwrap_or(0),
                    s.scope_latest.clone().unwrap_or_default(),
                )
            })
            .collect();
        if cwd_rows.is_empty() && filter.remote.is_empty() {
            return Err("No sessions in current directory.".to_string());
        }
        cwd_rows
    };

    if !filter.remote.is_empty() {
        let remotes = remote::resolve_remotes(&filter.remote)?;
        let remote_stats = remote::fetch_remote_agent_stats(&remotes, filter);
        for (remote_name, rs) in remote_stats {
            rows.push((rs.agent, remote_name, rs.sessions, rs.latest));
        }
    }

    if rows.is_empty() {
        return Err("No sessions found.".to_string());
    }

    let display_label = |agent: &str, remote: &str| -> String {
        if remote.is_empty() {
            agent.to_string()
        } else {
            format!("{} ({})", agent, remote)
        }
    };

    match output_format {
        cli::OutputFormat::Json => {
            for (agent, remote_name, count, latest) in &rows {
                let mut obj = serde_json::Map::new();
                obj.insert(
                    "agent".to_string(),
                    serde_json::Value::String(agent.clone()),
                );
                if !remote_name.is_empty() {
                    obj.insert(
                        "remote".to_string(),
                        serde_json::Value::String(remote_name.clone()),
                    );
                }
                obj.insert(
                    "sessions".to_string(),
                    serde_json::Value::Number((*count).into()),
                );
                obj.insert(
                    "latest".to_string(),
                    serde_json::Value::String(latest.clone()),
                );
                println!("{}", serde_json::Value::Object(obj));
            }
        }
        cli::OutputFormat::Ltsv => {
            for (agent, remote_name, count, latest) in &rows {
                if remote_name.is_empty() {
                    println!("agent:{}\tsessions:{}\tlatest:{}", agent, count, latest);
                } else {
                    println!(
                        "agent:{}\tremote:{}\tsessions:{}\tlatest:{}",
                        agent, remote_name, count, latest
                    );
                }
            }
        }
        cli::OutputFormat::Log | cli::OutputFormat::Table => {
            let is_tty = color::use_color();
            let (b, d, r) = if is_tty {
                (BOLD, DIM, RESET)
            } else {
                ("", "", "")
            };
            let labels: Vec<String> = rows
                .iter()
                .map(|(a, rn, _, _)| display_label(a, rn))
                .collect();
            let agent_w = labels.iter().map(|l| l.len()).max().unwrap_or(5).max(5);
            let count_w = rows
                .iter()
                .map(|(_, _, c, _)| c.to_string().len())
                .max()
                .unwrap_or(8)
                .max(8);
            println!(
                "{d}{:<agent_w$}  {:>count_w$}  LATEST{r}",
                "AGENT", "SESSIONS"
            );
            for (label, (_, _, count, latest)) in labels.iter().zip(rows.iter()) {
                println!(
                    "{b}{:<agent_w$}{r}  {:>count_w$}  {d}{}{r}",
                    label, count, latest
                );
            }
        }
        cli::OutputFormat::Tsv => {
            // Only emit the `remote` column when at least one row has it, so
            // local-only invocations keep the legacy 3-column shape.
            let has_remote = rows.iter().any(|(_, rn, _, _)| !rn.is_empty());
            for (agent, remote_name, count, latest) in &rows {
                if has_remote {
                    println!("{}\t{}\t{}\t{}", agent, remote_name, count, latest);
                } else {
                    println!("{}\t{}\t{}", agent, count, latest);
                }
            }
        }
    }
    Ok(())
}

//! `ah agent`: session counts per agent, overall and inside one directory.
use std::collections::BTreeMap;
use std::time::SystemTime;

use rayon::prelude::*;
use serde::Serialize;

use crate::agents::common::{canonical_home, format_mtime};
use crate::cli::{self, Field};
use crate::color::{self, BOLD, DIM, RESET};
use crate::pipeline::{SessionCopy, unique_session_copies};
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
/// that breakdown. Agents with no sessions are not listed. Files that hold
/// the same session count once, as in `ah log`.
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

    // Id and parent tell copies of one session apart from distinct sessions.
    let mut resolve_fields = vec![Field::Id, Field::ParentId];
    if scope_dir.is_some() {
        resolve_fields.push(Field::Cwd);
    }
    if filter.project.is_some() {
        resolve_fields.push(Field::Project);
    }

    struct Candidate<'a> {
        path: &'a std::path::Path,
        mtime: SystemTime,
        agent: &'static str,
        id: String,
        parent_id: String,
        in_cwd: bool,
    }
    let candidates: Vec<Candidate> = files
        .par_iter()
        .filter_map(|(path, mtime)| {
            if crate::cancel::checkpoint() {
                return None;
            }
            // Apply time filters
            if since.is_some_and(|since| *mtime < since)
                || until.is_some_and(|until| *mtime > until)
            {
                return None;
            }

            let agent = config::find_agent_for_path(path);
            let plugin = agent
                .map(|a| a.plugin)
                .unwrap_or_else(agents::unknown_plugin);
            let id = agent.map(|a| a.id.as_str()).unwrap_or("unknown");

            // Apply --agent filter
            if filter.agent.as_ref().is_some_and(|f| id != f) {
                return None;
            }

            let mut fields = resolver::resolve_fields(
                path,
                plugin,
                *mtime,
                &home,
                &resolve_fields,
                &Default::default(),
            );
            let field = |fields: &BTreeMap<Field, String>, f: Field| {
                fields.get(&f).map(|v| v.as_str()).unwrap_or("").to_string()
            };
            // Apply --project filter
            if filter
                .project
                .as_ref()
                .is_some_and(|p| field(&fields, Field::Project) != *p)
            {
                return None;
            }
            let in_cwd = scope_dir.is_some() && field(&fields, Field::Cwd) == cwd;
            Some(Candidate {
                path,
                mtime: *mtime,
                agent: id,
                id: fields.remove(&Field::Id).unwrap_or_default(),
                parent_id: fields.remove(&Field::ParentId).unwrap_or_default(),
                in_cwd,
            })
        })
        .collect();
    // Like `ah log`, which filters by directory before collapsing copies:
    // the directory's sessions are counted among the copies found there, so
    // a copy elsewhere that ranks higher does not hide them.
    let unique = |in_scope: bool| {
        unique_session_copies(
            candidates
                .iter()
                .filter(|c| !in_scope || c.in_cwd)
                .map(|c| SessionCopy {
                    path: c.path,
                    mtime: c.mtime,
                    id: &c.id,
                    parent_id: &c.parent_id,
                }),
        )
    };
    let new_stats = || AgentStats {
        total: 0,
        latest: SystemTime::UNIX_EPOCH,
        cwd_count: 0,
        cwd_latest: None,
    };

    let mut stats: BTreeMap<&str, AgentStats> = BTreeMap::new();
    for (c, _) in candidates
        .iter()
        .zip(unique(false))
        .filter(|(_, keep)| *keep)
    {
        let entry = stats.entry(c.agent).or_insert_with(new_stats);
        entry.total += 1;
        if c.mtime > entry.latest {
            entry.latest = c.mtime;
        }
    }
    if scope_dir.is_some() {
        let in_cwd = candidates.iter().filter(|c| c.in_cwd);
        for (c, _) in in_cwd.zip(unique(true)).filter(|(_, keep)| *keep) {
            let entry = stats.entry(c.agent).or_insert_with(new_stats);
            entry.cwd_count += 1;
            if entry.cwd_latest.is_none_or(|t| c.mtime > t) {
                entry.cwd_latest = Some(c.mtime);
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

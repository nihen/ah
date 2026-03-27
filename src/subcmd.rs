use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::agents;
use crate::cli::{Field, FieldFilter, SearchMode, SortOrder};
use crate::collector;
use crate::pipeline;
use crate::resolver;

struct ResolveLookupOpts {
    since: Option<SystemTime>,
    until: Option<SystemTime>,
    require_resume_cmd: bool,
}

/// Resolve a session file path from the given options.
///
/// Priority:
/// 1. Stdin pipe (path from pipe)
/// 2. Session (positional: ID or path)
/// 3. Query / filters → latest matching session (via pipeline)
pub fn resolve_session(
    session: Option<&str>,
    query: Option<&str>,
    filters: &[FieldFilter],
    home: &Path,
    search_mode: SearchMode,
    since: Option<SystemTime>,
    until: Option<SystemTime>,
) -> Result<PathBuf, String> {
    resolve_session_inner(
        session,
        query,
        filters,
        home,
        search_mode,
        ResolveLookupOpts {
            since,
            until,
            require_resume_cmd: false,
        },
    )
}

pub fn resolve_resumable_session(
    session: Option<&str>,
    query: Option<&str>,
    filters: &[FieldFilter],
    home: &Path,
    search_mode: SearchMode,
    since: Option<SystemTime>,
    until: Option<SystemTime>,
) -> Result<PathBuf, String> {
    resolve_session_inner(
        session,
        query,
        filters,
        home,
        search_mode,
        ResolveLookupOpts {
            since,
            until,
            require_resume_cmd: true,
        },
    )
}

fn resolve_session_inner(
    session: Option<&str>,
    query: Option<&str>,
    filters: &[FieldFilter],
    home: &Path,
    search_mode: SearchMode,
    opts: ResolveLookupOpts,
) -> Result<PathBuf, String> {
    if let Some(session_ref) = read_session_ref(session) {
        return resolve_session_ref(&session_ref, home);
    }

    // 3. Query / filters → latest via pipeline
    let q = query.unwrap_or("");
    let not_found_msg = if q.is_empty() {
        "No session found matching filters".to_string()
    } else {
        format!("No session found matching: {}", q)
    };

    let result = pipeline::run_pipeline(&pipeline::PipelineParams {
        resolve_fields: resolve_fields_for_lookup(opts.require_resume_cmd),
        resolve_opts: resolver::ResolveOpts::default(),
        filters: filters.to_vec(),
        since: opts.since,
        until: opts.until,
        query: q.to_string(),
        search_mode,
        sort_field: Field::ModifiedAt,
        sort_order: SortOrder::Desc,
        collect_limit: 0, // scan all: filter/search runs after collect
        running: false,
        require_resume_cmd: opts.require_resume_cmd,
    })?;

    match result.sessions.into_iter().next() {
        Some(s) => Ok(s.path),
        None => Err(not_found_msg),
    }
}

/// Read an explicit session reference from stdin or positional argument.
/// Stdin takes precedence over the positional argument when present.
pub fn read_session_ref(session: Option<&str>) -> Option<String> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut line = String::new();
        let mut stdin = std::io::stdin().lock();
        if std::io::BufRead::read_line(&mut stdin, &mut line).is_ok() {
            let line = line.trim();
            if !line.is_empty() {
                let first_field = line.split('\t').next().unwrap_or(line);
                let session_ref = normalize_session_ref(first_field);
                if !session_ref.is_empty() {
                    return Some(session_ref);
                }
            }
        }
    }

    session
        .map(normalize_session_ref)
        .filter(|session_ref| !session_ref.is_empty())
}

/// Resolve a session reference: try as file path first, then as session ID.
pub fn resolve_session_ref(s: &str, home: &Path) -> Result<PathBuf, String> {
    let s = strip_ltsv_prefix(s);

    // Try as file path
    let pb = PathBuf::from(s);
    if pb.exists() {
        return Ok(pb);
    }

    // Strip surrounding quotes (e.g. from fzf preview passing shell-quoted paths)
    let unquoted = crate::output::strip_quotes(s);
    if unquoted != s {
        let pb = PathBuf::from(unquoted);
        if pb.exists() {
            return Ok(pb);
        }
    }

    // Try as session ID (use unquoted value)
    resolve_by_id(unquoted, home)
}

fn resolve_by_id(id: &str, home: &Path) -> Result<PathBuf, String> {
    let files = collector::collect_files(0);
    let resolve_fields = [Field::Id];
    let opts = resolver::ResolveOpts::default();

    let mut prefix_match: Option<PathBuf> = None;
    let mut prefix_ambiguous = false;

    for (fpath, mtime) in &files {
        let plugin = agents::find_plugin_for_path(fpath);
        let fields = resolver::resolve_fields(fpath, plugin, *mtime, home, &resolve_fields, &opts);
        if let Some(v) = fields.get(&Field::Id) {
            if v == id {
                return Ok(fpath.clone());
            }
            if v.starts_with(id) {
                if prefix_match.is_some() {
                    prefix_ambiguous = true;
                } else {
                    prefix_match = Some(fpath.clone());
                }
            }
        }
    }

    if prefix_ambiguous {
        return Err(format!("Ambiguous session id prefix: {}", id));
    }
    if let Some(path) = prefix_match {
        return Ok(path);
    }
    Err(format!("No session found for id: {}", id))
}

fn normalize_session_ref(s: &str) -> String {
    let s = strip_ltsv_prefix(s);
    crate::output::strip_ansi(s).trim().to_string()
}

fn strip_ltsv_prefix(s: &str) -> &str {
    strip_ltsv_prefix_with(s, |candidate| {
        crate::remote::parse_remote_path(candidate).is_some()
    })
}

fn strip_ltsv_prefix_with<F>(s: &str, is_remote_ref: F) -> &str
where
    F: Fn(&str) -> bool,
{
    if is_remote_ref(s) {
        return s;
    }

    let Some(i) = s.find(':') else {
        return s;
    };
    let after = &s[i + 1..];

    if is_remote_ref(after) || after.starts_with('/') {
        after
    } else {
        s
    }
}

fn resolve_fields_for_lookup(require_resume_cmd: bool) -> Vec<Field> {
    let mut fields = vec![Field::Path, Field::ModifiedAt];
    if require_resume_cmd {
        fields.push(Field::ResumeCmd);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumable_lookup_resolves_resume_cmd_field() {
        assert_eq!(
            resolve_fields_for_lookup(true),
            vec![Field::Path, Field::ModifiedAt, Field::ResumeCmd]
        );
    }

    #[test]
    fn non_resumable_lookup_keeps_default_fields() {
        assert_eq!(
            resolve_fields_for_lookup(false),
            vec![Field::Path, Field::ModifiedAt]
        );
    }

    #[test]
    fn strip_ltsv_prefix_keeps_remote_refs() {
        assert_eq!(
            strip_ltsv_prefix_with("mydev:/tmp/session.jsonl", |s| s.starts_with("mydev:/")),
            "mydev:/tmp/session.jsonl"
        );
    }

    #[test]
    fn strip_ltsv_prefix_preserves_remote_refs_inside_ltsv_values() {
        assert_eq!(
            strip_ltsv_prefix_with("path:mydev:/tmp/session.jsonl", |s| s
                .starts_with("mydev:/")),
            "mydev:/tmp/session.jsonl"
        );
    }
}

use std::time::{Instant, SystemTime};

use rayon::prelude::*;
use regex::Regex;
use regex::bytes::Regex as BytesRegex;

use crate::agents;
use crate::agents::common::mmap_file;
use crate::cli::{Field, FieldFilter, SearchMode, SortOrder};
use crate::collector;
use crate::color;
use crate::resolver::{self, ResolveOpts};
use crate::search;
use crate::session::Session;

/// Parameters controlling the unified session pipeline.
pub struct PipelineParams {
    pub resolve_fields: Vec<Field>,
    pub resolve_opts: ResolveOpts,
    pub filters: Vec<FieldFilter>,
    pub since: Option<SystemTime>,
    pub until: Option<SystemTime>,
    pub query: String,
    pub search_mode: SearchMode,
    pub sort_field: Field,
    pub sort_order: SortOrder,
    pub collect_limit: usize,
    pub running: bool,
    pub require_resume_cmd: bool,
}

/// Result of the pipeline execution.
pub struct PipelineResult {
    pub sessions: Vec<Session>,
    pub pid_map: std::collections::HashMap<String, u32>,
}

/// Unified session pipeline: collect → filter → resolve → sort.
///
/// All session-listing code paths (log, show -i, resume -i, find_latest_matching)
/// use this single function so that filters behave identically everywhere.
pub fn run_pipeline(params: &PipelineParams) -> Result<PipelineResult, String> {
    let debug = color::is_debug();
    let t0 = Instant::now();

    let home = agents::common::canonical_home();
    let files = collector::collect_files(params.collect_limit);

    if debug {
        eprintln!(
            "[debug] pipeline: {} files collected  ({:.1}ms)",
            files.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
        if !params.query.is_empty() {
            eprintln!(
                "[debug] pipeline: query={:?} mode={:?}",
                params.query, params.search_mode
            );
        }
        if !params.filters.is_empty() {
            for f in &params.filters {
                eprintln!("[debug] pipeline: filter {}={:?}", f.field.name(), f.value);
            }
        }
    }

    let has_query = !params.query.is_empty();
    let bytes_pattern = if has_query && params.search_mode == SearchMode::All {
        Some(compile_bytes_regex(&params.query)?)
    } else {
        None
    };
    let text_pattern = if has_query && params.search_mode == SearchMode::Prompt {
        Some(compile_text_regex(&params.query)?)
    } else {
        None
    };

    let mut resolve_fields = params.resolve_fields.clone();
    if !resolve_fields.contains(&Field::ModifiedAt) {
        resolve_fields.push(Field::ModifiedAt);
    }
    if !resolve_fields.contains(&Field::Id) {
        resolve_fields.push(Field::Id);
    }
    FieldFilter::ensure_fields(&params.filters, &mut resolve_fields);

    // Split filters into early (cheap) and late (need full resolution).
    // Cwd and Agent can be resolved cheaply without full field resolution.
    let early_filter_fields: Vec<Field> = params
        .filters
        .iter()
        .filter(|f| matches!(f.field, Field::Cwd | Field::Agent))
        .map(|f| f.field)
        .collect();
    let has_early_filters = !early_filter_fields.is_empty();
    let early_filters: Vec<&FieldFilter> = params
        .filters
        .iter()
        .filter(|f| matches!(f.field, Field::Cwd | Field::Agent))
        .collect();
    let late_filters: Vec<&FieldFilter> = params
        .filters
        .iter()
        .filter(|f| !matches!(f.field, Field::Cwd | Field::Agent))
        .collect();

    // Pre-compute fast literal needle for ASCII queries without regex metacharacters.
    // Uses memchr SIMD search instead of regex for massive speedup.
    let fast_needle: Option<Vec<u8>> = if has_query
        && params.search_mode == SearchMode::All
        && params.query.is_ascii()
        && !params.query.chars().any(|c| {
            matches!(
                c,
                '\\' | '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
            )
        }) {
        Some(
            params
                .query
                .as_bytes()
                .iter()
                .map(|b| b.to_ascii_lowercase())
                .collect(),
        )
    } else {
        None
    };

    let mut sessions: Vec<Session> = files
        .par_iter()
        .filter_map(|(path, mtime)| {
            // Time range filter
            if let Some(since) = &params.since {
                if mtime < since {
                    return None;
                }
            }
            if let Some(until) = &params.until {
                if mtime > until {
                    return None;
                }
            }

            let plugin = agents::find_plugin_for_path(path);

            // Mmap the search file once — shared across search, resolve_matched,
            // resolve_title, and resolve_cwd.
            let search_path = plugin.search_path(path);
            let is_session_file = search_path == *path;
            let mmap = mmap_file(&search_path);

            // Early field filters (cwd, agent) — cheap, before query search.
            // Use mmap for cwd resolution when the search file IS the session file.
            if has_early_filters {
                let session_mmap = if is_session_file {
                    mmap.as_deref()
                } else {
                    None
                };
                let early_fields = resolver::resolve_fields_with_mmap(
                    path,
                    plugin,
                    *mtime,
                    &home,
                    &early_filter_fields,
                    &params.resolve_opts,
                    session_mmap,
                );
                for f in &early_filters {
                    if early_fields.get(&f.field).map(|v| v.as_str()).unwrap_or("") != f.value {
                        return None;
                    }
                }
            }

            // Query search using pre-loaded mmap
            if has_query {
                let matches = match params.search_mode {
                    SearchMode::Prompt => text_pattern
                        .as_ref()
                        .is_some_and(|re| search::search_prompts_matches(path, plugin, re)),
                    SearchMode::All => match &mmap {
                        Some(m) => {
                            if let Some(needle) = &fast_needle {
                                ascii_case_insensitive_contains(m, needle)
                            } else {
                                bytes_pattern.as_ref().is_some_and(|re| re.is_match(m))
                            }
                        }
                        None => false,
                    },
                };
                if !matches {
                    return None;
                }
            }

            // Resolve fields, passing mmap for reuse by resolve_matched/title/cwd
            let session_mmap = if is_session_file {
                mmap.as_deref()
            } else {
                None
            };
            let fields = resolver::resolve_fields_with_mmap(
                path,
                plugin,
                *mtime,
                &home,
                &resolve_fields,
                &params.resolve_opts,
                session_mmap,
            );

            // Skip if Matched was requested but is empty
            if has_query
                && resolve_fields.contains(&Field::Matched)
                && fields.get(&Field::Matched).is_none_or(|v| v.is_empty())
            {
                return None;
            }

            // Late field filters (--project etc.)
            for f in &late_filters {
                if fields.get(&f.field).map(|v| v.as_str()).unwrap_or("") != f.value {
                    return None;
                }
            }

            // Resume command required check
            if params.require_resume_cmd
                && fields.get(&Field::ResumeCmd).is_none_or(|v| v.is_empty())
            {
                return None;
            }

            Some(Session {
                path: path.clone(),
                fields,
            })
        })
        .collect();

    if debug {
        eprintln!(
            "[debug] pipeline: {} sessions after filter+resolve  ({:.1}ms)",
            sessions.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    // PID map + running field (Claude only for now)
    let pid_map = crate::build_pid_map();
    for session in &mut sessions {
        let session_id = session.fields.get(&Field::Id).cloned().unwrap_or_default();
        if let Some(&pid) = pid_map.get(&session_id) {
            session.fields.insert(Field::Pid, pid.to_string());
            session.fields.insert(Field::Running, "true".to_string());
        } else {
            session.fields.insert(Field::Running, "false".to_string());
        }
    }
    if params.running {
        sessions.retain(|s| s.fields.get(&Field::Running).is_some_and(|v| v == "true"));
    }

    // Sort
    let numeric = params.sort_field.is_numeric();
    match params.sort_order {
        SortOrder::Desc => sessions.sort_by(|a, b| {
            crate::output::compare_field_values(
                b.fields.get(&params.sort_field),
                a.fields.get(&params.sort_field),
                numeric,
            )
        }),
        SortOrder::Asc => sessions.sort_by(|a, b| {
            crate::output::compare_field_values(
                a.fields.get(&params.sort_field),
                b.fields.get(&params.sort_field),
                numeric,
            )
        }),
    }

    // Deduplicate sessions with the same ID.
    // When a tool (e.g. copilot) is invoked from within another agent, both may
    // record the same session ID. Prefer the entry whose file path matches its agent.
    {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, session) in sessions.iter().enumerate() {
            let id = session
                .fields
                .get(&Field::Id)
                .map(|v| v.as_str())
                .unwrap_or("");
            if id.is_empty() {
                continue;
            }
            if seen.contains_key(id) {
                // Keep the entry whose path matches the agent detected from path_markers
                let path_agent = crate::config::find_agent_for_path(&session.path)
                    .map(|a| a.id.as_str())
                    .unwrap_or("");
                let field_agent = session
                    .fields
                    .get(&Field::Agent)
                    .map(|v| v.as_str())
                    .unwrap_or("");
                // Replace only if this entry's agent matches the session's agent field
                // (i.e., this is the true owner, not a cross-agent duplicate).
                // If Agent field was not resolved, keep first-seen entry.
                if !field_agent.is_empty() && path_agent == field_agent {
                    seen.insert(id.to_string(), i);
                }
                // Otherwise keep previous entry (first seen or true owner)
            } else {
                seen.insert(id.to_string(), i);
            }
        }
        let keep: std::collections::HashSet<usize> = seen.values().copied().collect();
        let mut idx = 0;
        sessions.retain(|_| {
            let k = keep.contains(&idx);
            idx += 1;
            k
        });
    }

    if debug {
        eprintln!(
            "[debug] pipeline: {} sessions final  ({:.1}ms total)",
            sessions.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    Ok(PipelineResult { sessions, pid_map })
}

/// Fast case-insensitive byte search for ASCII patterns using SIMD-accelerated memchr.
fn ascii_case_insensitive_contains(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let first_lower = needle_lower[0];
    let first_upper = first_lower.to_ascii_uppercase();
    let has_case = first_lower != first_upper;

    let mut start = 0;
    loop {
        if start + needle_lower.len() > haystack.len() {
            return false;
        }
        let found = if has_case {
            memchr::memchr2(first_lower, first_upper, &haystack[start..])
        } else {
            memchr::memchr(first_lower, &haystack[start..])
        };
        let Some(pos) = found else { return false };
        let abs = start + pos;
        if abs + needle_lower.len() > haystack.len() {
            return false;
        }
        if haystack[abs..abs + needle_lower.len()]
            .iter()
            .zip(needle_lower)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
        {
            return true;
        }
        start = abs + 1;
    }
}

fn compile_bytes_regex(query: &str) -> Result<BytesRegex, String> {
    BytesRegex::new(&format!("(?iu){}", query))
        .map_err(|e| format!("Invalid regex '{}': {}", query, e))
}

fn compile_text_regex(query: &str) -> Result<Regex, String> {
    Regex::new(&format!("(?i){}", query)).map_err(|e| format!("Invalid regex '{}': {}", query, e))
}

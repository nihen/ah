use std::path::Path;
use std::time::{Instant, SystemTime};

use rayon::prelude::*;
use regex::Regex;
use regex::bytes::Regex as BytesRegex;

use crate::agents;
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
    let raw_search = params.search_mode == SearchMode::Raw;
    let bytes_pattern = if has_query && raw_search {
        Some(compile_bytes_regex(&params.query)?)
    } else {
        None
    };
    let text_pattern = if has_query && !raw_search {
        Some(compile_text_regex(&params.query)?)
    } else {
        None
    };
    let query_prefilter = if has_query && !raw_search {
        search::QueryPrefilter::new(&params.query)
    } else {
        None
    };
    let prompt_only = params.search_mode == SearchMode::Prompt;
    let line_check = if has_query && !raw_search && !prompt_only {
        search::LineCheck::new(&params.query)
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
    if !resolve_fields.contains(&Field::ParentId) {
        resolve_fields.push(Field::ParentId);
    }
    FieldFilter::ensure_fields(&params.filters, &mut resolve_fields);
    let wants_matched = resolve_fields.contains(&Field::Matched);
    let resolve_fields_but_matched: Vec<Field> = resolve_fields
        .iter()
        .copied()
        .filter(|f| *f != Field::Matched)
        .collect();

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
        && raw_search
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

    let sessions: Vec<(Session, SystemTime)> = files
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
            // resolve_title, and resolve_cwd. Plugins with expensive session
            // bytes (e.g. SQLite exports) load them after the early filters,
            // and only for full-text search.
            let search_path = plugin.search_path(path);
            let is_session_file = search_path == *path;
            let mut mmap = if plugin.cheap_session_bytes() {
                plugin.session_bytes(path)
            } else {
                None
            };

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

            if mmap.is_none() && has_query && raw_search && !plugin.cheap_session_bytes() {
                mmap = plugin.session_bytes(path);
            }

            // Query search using pre-loaded mmap. Message search yields the
            // match context, reused as the `matched` field.
            let mut matched = None;
            if has_query {
                let matches = if raw_search {
                    match &mmap {
                        Some(m) => {
                            if let Some(needle) = &fast_needle {
                                ascii_case_insensitive_contains(m, needle)
                            } else {
                                bytes_pattern.as_ref().is_some_and(|re| re.is_match(m))
                            }
                        }
                        None => false,
                    }
                } else {
                    matched = text_pattern.as_ref().and_then(|re| {
                        let data = if is_session_file {
                            mmap.as_deref()
                        } else {
                            None
                        };
                        if prompt_only {
                            if let (Some(prefilter), Some(m)) = (&query_prefilter, mmap.as_deref())
                            {
                                if plugin.prompts_per_jsonl_line() && is_session_file {
                                    let lines = prefilter.candidate_lines(m)?;
                                    return search::search_prompts(
                                        path,
                                        plugin,
                                        re,
                                        Some(lines.as_ref()),
                                    );
                                }
                                if plugin.prompts_in_session_json() && !prefilter.may_match(m) {
                                    return None;
                                }
                            }
                            return search::search_prompts(path, plugin, re, data);
                        }
                        // Search texts are read from `session_bytes`, loaded
                        // here as `mmap` when cheap.
                        if let (Some(prefilter), Some(m)) = (&query_prefilter, mmap.as_deref()) {
                            if plugin.search_texts_per_jsonl_line() {
                                // Parse candidate lines one at a time and stop
                                // at the first match.
                                let mut found = None;
                                let keep = |line: &[u8]| plugin.line_may_hold_search_texts(line);
                                prefilter.for_each_candidate_line(m, keep, |line| {
                                    if line_check.as_ref().is_some_and(|c| !c.may_match(line)) {
                                        return true;
                                    }
                                    found = search::search_texts(path, plugin, re, Some(line));
                                    found.is_none()
                                });
                                return found;
                            }
                            if plugin.search_texts_in_session_json() && !prefilter.may_match(m) {
                                return None;
                            }
                        }
                        search::search_texts(path, plugin, re, mmap.as_deref())
                    });
                    matched.is_some()
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
            let mut fields = resolver::resolve_fields_with_mmap(
                path,
                plugin,
                *mtime,
                &home,
                if matched.is_some() {
                    &resolve_fields_but_matched
                } else {
                    &resolve_fields
                },
                &params.resolve_opts,
                session_mmap,
            );
            if let Some(matched) = matched {
                if wants_matched {
                    fields.insert(Field::Matched, matched);
                }
            }

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

            Some((
                Session {
                    path: path.clone(),
                    fields,
                },
                *mtime,
            ))
        })
        .collect();
    let mut sessions = dedup_by_id(sessions);

    if debug {
        eprintln!(
            "[debug] pipeline: {} sessions after filter+resolve  ({:.1}ms)",
            sessions.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    // Running sessions reported by the agent plugins
    let pid_map = crate::agents::running_session_map();
    for session in &mut sessions {
        let session_id = session.fields.get(&Field::Id).cloned().unwrap_or_default();
        if let Some(pid) = pid_map.get(&session_id) {
            if let Some(pid) = pid {
                session.fields.insert(Field::Pid, pid.to_string());
            }
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

    if debug {
        eprintln!(
            "[debug] pipeline: {} sessions final  ({:.1}ms total)",
            sessions.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    Ok(PipelineResult { sessions })
}

/// Preference among files that share a session id: a dedicated session file
/// over a secondary record, then the most recently modified, then the path
/// (for a stable choice). Shared by `log` and id lookups so both pick the
/// same copy (e.g. a Gemini session migrated from .json to .jsonl).
pub fn copy_preference(path: &Path, mtime: SystemTime) -> (bool, SystemTime, &Path) {
    let secondary = agents::find_plugin_for_path(path).is_secondary_record(path);
    (!secondary, mtime, path)
}

/// Keep one session per id (and parent, so subagents of different sessions
/// that share an id stay distinct), the copy `copy_preference` ranks
/// highest. Sessions without an id are distinct and all kept.
fn dedup_by_id(sessions: Vec<(Session, SystemTime)>) -> Vec<Session> {
    fn field(session: &Session, field: Field) -> &str {
        session.fields.get(&field).map(|v| v.as_str()).unwrap_or("")
    }
    let mut best: std::collections::HashMap<(&str, &str), usize> = std::collections::HashMap::new();
    for (i, (session, mtime)) in sessions.iter().enumerate() {
        let id = field(session, Field::Id);
        if id.is_empty() {
            continue;
        }
        best.entry((id, field(session, Field::ParentId)))
            .and_modify(|j| {
                let (other, other_mtime) = &sessions[*j];
                if copy_preference(&session.path, *mtime)
                    > copy_preference(&other.path, *other_mtime)
                {
                    *j = i;
                }
            })
            .or_insert(i);
    }
    let keep: std::collections::HashSet<usize> = best.into_values().collect();
    sessions
        .into_iter()
        .enumerate()
        .filter(|(i, (session, _))| {
            keep.contains(i) || session.fields.get(&Field::Id).is_none_or(|v| v.is_empty())
        })
        .map(|(_, (session, _))| session)
        .collect()
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

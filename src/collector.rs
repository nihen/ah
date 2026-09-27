use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Instant, SystemTime};

use rayon::prelude::*;

use crate::agents::{self, AgentPlugin};
use crate::color;
use crate::config;

static EXCLUDE_ARCHIVED: AtomicBool = AtomicBool::new(false);
static INCLUDE_SUBAGENTS: AtomicBool = AtomicBool::new(false);

/// Hide archived sessions from listings (`--no-archived`).
pub fn init_exclude_archived(exclude: bool) {
    EXCLUDE_ARCHIVED.store(exclude, AtomicOrdering::Relaxed);
}

/// List subagent sessions too (`--subagents`).
pub fn init_include_subagents(include: bool) {
    INCLUDE_SUBAGENTS.store(include, AtomicOrdering::Relaxed);
}

/// How a collection treats subagent sessions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Subagents {
    /// Skip sessions a plugin reports as subagents (the listing default).
    Hide,
    /// Keep whatever the main patterns match, without the subagent patterns.
    Unfiltered,
    /// Also collect the plugins' subagent patterns (`--subagents`, lookups).
    Include,
}

/// Collect session files, sorted by mtime descending, limited to top N.
/// Archived sessions are skipped when `--no-archived` is set, and subagent
/// sessions unless `--subagents` is set.
pub fn collect_files(limit: usize) -> Vec<(PathBuf, SystemTime)> {
    let subagents = if INCLUDE_SUBAGENTS.load(AtomicOrdering::Relaxed) {
        Subagents::Include
    } else {
        Subagents::Hide
    };
    collect(
        limit,
        EXCLUDE_ARCHIVED.load(AtomicOrdering::Relaxed),
        subagents,
    )
}

/// Like `collect_files`, but always includes archived and subagent sessions
/// (for lookups of an explicit session reference).
pub fn collect_all_files(limit: usize) -> Vec<(PathBuf, SystemTime)> {
    collect(limit, false, Subagents::Include)
}

/// Every session the main patterns match, archived and subagent ones
/// included, for discovering project directories: listing flags hide
/// sessions, not the projects they reveal.
pub fn collect_project_files(limit: usize) -> Vec<(PathBuf, SystemTime)> {
    collect(limit, false, Subagents::Unfiltered)
}

/// Newest first; equal mtimes by path descending, so the order (and the copy
/// an id lookup picks) is stable across runs.
fn newest_first(a: &(PathBuf, SystemTime), b: &(PathBuf, SystemTime)) -> Ordering {
    b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0))
}

fn collect(
    limit: usize,
    exclude_archived: bool,
    subagents: Subagents,
) -> Vec<(PathBuf, SystemTime)> {
    let hide_subagents = subagents == Subagents::Hide;
    let debug = color::is_debug();
    let t0 = if debug { Some(Instant::now()) } else { None };

    // (plugin, pattern, whether matches must be checked to map back to the plugin)
    let patterns: Vec<(&'static dyn AgentPlugin, String, bool)> = config::active_agents()
        .flat_map(|agent| {
            let subagent_patterns = if subagents == Subagents::Include {
                agent.subagent_patterns.as_slice()
            } else {
                &[]
            };
            agent
                .glob_patterns
                .iter()
                .map(move |pattern| {
                    (
                        agent.plugin,
                        pattern.clone(),
                        agent.unattributed_patterns.contains(pattern),
                    )
                })
                .chain(
                    subagent_patterns
                        .iter()
                        .map(move |pattern| (agent.plugin, pattern.clone(), false)),
                )
        })
        .collect();

    let per_pattern: Vec<Vec<PathBuf>> = patterns
        .par_iter()
        .map(|(_, pattern, _)| {
            glob::glob(pattern)
                .map(|entries| entries.flatten().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();

    if debug {
        for ((_, pattern, _), paths) in patterns.iter().zip(per_pattern.iter()) {
            eprintln!("[debug] glob {:>5} files  {}", paths.len(), pattern);
        }
    }

    // Container files (e.g. SQLite databases) expand into virtual sessions
    // that already carry their own mtime; plain files are stat'ed below.
    // The same session in several containers (e.g. a backup database) is
    // listed once, from the most recently updated copy.
    let mut expanded: HashSet<PathBuf> = HashSet::new();
    let mut virtual_entries: HashMap<(&'static str, OsString), (PathBuf, SystemTime)> =
        HashMap::new();
    let per_pattern: Vec<Vec<PathBuf>> = patterns
        .iter()
        .zip(per_pattern)
        .map(|((plugin, _, unattributed), paths)| {
            let owned = |path: &PathBuf| config::find_plugin_for_path(path).id() == plugin.id();
            let mut files = Vec::new();
            for path in paths {
                // A match of an extra pattern without its own marker that does
                // not map back to this plugin could not be parsed later, so it
                // is skipped (and never opened) instead of listed as unknown.
                if expanded.contains(&path) || (*unattributed && !owned(&path)) {
                    continue;
                }
                match plugin.expand_sessions(&path) {
                    Some(sessions) => {
                        for (session, mtime) in sessions {
                            // An unowned copy must not win over a good one.
                            if !owned(&session)
                                || (exclude_archived && plugin.is_archived(&session))
                                || (hide_subagents && plugin.is_subagent(&session))
                            {
                                continue;
                            }
                            let key = (
                                plugin.id(),
                                session.file_name().unwrap_or_default().to_owned(),
                            );
                            match virtual_entries.entry(key) {
                                Entry::Occupied(mut e) => {
                                    if (mtime, &session) > (e.get().1, &e.get().0) {
                                        e.insert((session, mtime));
                                    }
                                }
                                Entry::Vacant(e) => {
                                    e.insert((session, mtime));
                                }
                            }
                        }
                        expanded.insert(path);
                    }
                    None => files.push(path),
                }
            }
            files
        })
        .collect();

    let all_paths: HashSet<PathBuf> = per_pattern.into_iter().flatten().collect();
    let unique_count = all_paths.len();

    // Parallel stat. The plugin the file is attributed to decides the
    // session's mtime (e.g. Copilot's events.jsonl is newer than the
    // workspace.yaml the glob matched), so time filters, `-n` and project
    // dates agree with `modified_at`.
    let mut entries: Vec<(PathBuf, SystemTime)> = all_paths
        .into_par_iter()
        .filter_map(|path| {
            let plugin = config::find_plugin_for_path(&path);
            // No active owner (e.g. a file attributed to a disabled agent):
            // it could not be parsed, and disabled agents stay hidden.
            if plugin.id() == agents::unknown_plugin().id()
                || (exclude_archived && plugin.is_archived(&path))
                || (hide_subagents && plugin.is_subagent(&path))
            {
                return None;
            }
            plugin.session_mtime(&path).map(|mtime| (path, mtime))
        })
        .collect();
    let mut virtual_entries: Vec<_> = virtual_entries.into_values().collect();
    virtual_entries.sort();
    entries.extend(virtual_entries);

    if debug {
        eprintln!(
            "[debug] collector: {} unique files, {} after stat, limit={}  ({:.1}ms)",
            unique_count,
            entries.len(),
            if limit == 0 {
                "none".to_string()
            } else {
                limit.to_string()
            },
            t0.unwrap().elapsed().as_secs_f64() * 1000.0,
        );
    }

    // No limit: sort and return all
    if limit == 0 || limit >= entries.len() {
        let mut sorted = entries;
        sorted.sort_by(newest_first);
        return sorted;
    }

    // Top-N via min-heap
    struct MinEntry {
        path: PathBuf,
        mtime: SystemTime,
    }
    impl PartialEq for MinEntry {
        fn eq(&self, other: &Self) -> bool {
            self.mtime == other.mtime && self.path == other.path
        }
    }
    impl Eq for MinEntry {}
    impl Ord for MinEntry {
        fn cmp(&self, other: &Self) -> Ordering {
            other
                .mtime
                .cmp(&self.mtime)
                .then_with(|| other.path.cmp(&self.path))
        }
    }
    impl PartialOrd for MinEntry {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut heap: BinaryHeap<MinEntry> = BinaryHeap::with_capacity(limit + 1);
    for (path, mtime) in entries {
        heap.push(MinEntry { path, mtime });
        if heap.len() > limit {
            heap.pop();
        }
    }

    let mut result: Vec<_> = heap.into_iter().map(|e| (e.path, e.mtime)).collect();
    result.sort_by(newest_first);
    result
}

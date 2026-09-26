use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use rayon::prelude::*;

use crate::agents::AgentPlugin;
use crate::color;
use crate::config;

/// Collect session files, sorted by mtime descending, limited to top N.
pub fn collect_files(limit: usize) -> Vec<(PathBuf, SystemTime)> {
    let debug = color::is_debug();
    let t0 = if debug { Some(Instant::now()) } else { None };

    let patterns: Vec<(&'static dyn AgentPlugin, String)> = config::active_agents()
        .flat_map(|agent| {
            agent
                .glob_patterns
                .iter()
                .map(move |pattern| (agent.plugin, pattern.clone()))
        })
        .collect();

    let per_pattern: Vec<Vec<PathBuf>> = patterns
        .par_iter()
        .map(|(_, pattern)| {
            glob::glob(pattern)
                .map(|entries| entries.flatten().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();

    if debug {
        for ((_, pattern), paths) in patterns.iter().zip(per_pattern.iter()) {
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
        .map(|((plugin, _), paths)| {
            let mut files = Vec::new();
            for path in paths {
                if expanded.contains(&path) {
                    continue;
                }
                match plugin.expand_sessions(&path) {
                    Some(sessions) => {
                        for (session, mtime) in sessions {
                            // A copy whose path does not map back to this plugin
                            // (e.g. an extra pattern directly under HOME) could not
                            // be parsed later, so it must not win over a good one.
                            if config::find_plugin_for_path(&session).id() != plugin.id() {
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

    // Parallel stat
    let mut entries: Vec<(PathBuf, SystemTime)> = all_paths
        .into_par_iter()
        .filter_map(|path| {
            fs::metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok().map(|mtime| (path, mtime)))
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
        sorted.sort_by_key(|e| std::cmp::Reverse(e.1));
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
    result.sort_by_key(|e| std::cmp::Reverse(e.1));
    result
}

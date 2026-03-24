use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use rayon::prelude::*;

use crate::color;
use crate::config;

/// Collect session files, sorted by mtime descending, limited to top N.
pub fn collect_files(limit: usize) -> Vec<(PathBuf, SystemTime)> {
    let debug = color::is_debug();
    let t0 = if debug { Some(Instant::now()) } else { None };

    let patterns: Vec<String> = config::active_agents()
        .flat_map(|agent| agent.glob_patterns.iter().cloned())
        .collect();

    let per_pattern: Vec<Vec<PathBuf>> = patterns
        .par_iter()
        .map(|pattern| {
            glob::glob(pattern)
                .map(|entries| entries.flatten().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();

    if debug {
        for (pattern, paths) in patterns.iter().zip(per_pattern.iter()) {
            eprintln!("[debug] glob {:>5} files  {}", paths.len(), pattern);
        }
    }

    let all_paths: HashSet<PathBuf> = per_pattern.into_iter().flatten().collect();
    let unique_count = all_paths.len();

    // Parallel stat
    let entries: Vec<(PathBuf, SystemTime)> = all_paths
        .into_par_iter()
        .filter_map(|path| {
            fs::metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok().map(|mtime| (path, mtime)))
        })
        .collect();

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
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
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
    result.sort_by(|a, b| b.1.cmp(&a.1));
    result
}

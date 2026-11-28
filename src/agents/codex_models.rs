//! Models of compressed Codex rollouts, kept on disk between runs.
//!
//! Finding the models of a rollout reads all of it, which for a compressed
//! one means decoding it. Codex does not append to a compressed rollout (it
//! decompresses it first), so its models stay the same while its size and
//! mtime do. Each line of the cache file is
//! `<size>\t<mtime ns>\t<model>\x1f<model>...\t<path>`; lines are only
//! appended, the last line for a path wins, and lines that cannot be read
//! are ignored.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::UNIX_EPOCH;

use super::common::ModelList;

/// Separates the models of one line (models are never control characters).
const MODEL_SEPARATOR: char = '\x1f';

/// Size and mtime (nanoseconds since the epoch) of a rollout.
type Stamp = (u64, u128);

struct Cache {
    file: Option<PathBuf>,
    entries: HashMap<PathBuf, (Stamp, Vec<String>)>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
    // Unit tests do not write to the user's cache.
    let file = dirs::cache_dir()
        .filter(|_| !cfg!(test))
        .map(|dir| dir.join("ah").join("codex-zst-models-v1.tsv"));
    let entries = file.as_deref().map(read_entries).unwrap_or_default();
    Mutex::new(Cache { file, entries })
});

fn read_entries(file: &Path) -> HashMap<PathBuf, (Stamp, Vec<String>)> {
    let mut entries = HashMap::new();
    let Ok(text) = std::fs::read_to_string(file) else {
        return entries;
    };
    for line in text.lines() {
        let mut fields = line.splitn(4, '\t');
        let (Some(size), Some(mtime), Some(models), Some(path)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(size), Ok(mtime)) = (size.parse(), mtime.parse()) else {
            continue;
        };
        let models = models
            .split(MODEL_SEPARATOR)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect();
        entries.insert(PathBuf::from(path), ((size, mtime), models));
    }
    entries
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((meta.len(), mtime.as_nanos()))
}

/// The pushes that rebuild a list: its models in first-use order, then the
/// one that answered last when that is not the last of them.
fn pushes(models: &ModelList) -> Vec<String> {
    let mut pushes: Vec<String> = models.all().to_vec();
    let last = models.last();
    if !last.is_empty() && pushes.last().is_none_or(|m| m != last) {
        pushes.push(last.to_string());
    }
    pushes
}

/// The models of the compressed rollout at `path`: from the cache while its
/// size and mtime are unchanged, else from `scan`, whose result is kept.
pub(super) fn models(path: &Path, scan: impl FnOnce() -> ModelList) -> Option<ModelList> {
    let rebuild = |pushes: &[String]| {
        let mut models = ModelList::default();
        for model in pushes {
            models.push(model);
        }
        models.non_empty()
    };
    let Some(stamp) = stamp(path) else {
        return scan().non_empty();
    };
    if let Some((cached, pushes)) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .get(path)
        && *cached == stamp
    {
        return rebuild(pushes);
    }
    let models = scan();
    if crate::cancel::requested() {
        // A cancelled scan may have stopped early.
        return models.non_empty();
    }
    let pushes = pushes(&models);
    let line = (|| {
        let path = path.to_str()?;
        let fine = |s: &str| !s.contains(['\t', '\n', '\r', MODEL_SEPARATOR]);
        (fine(path) && pushes.iter().all(|m| fine(m))).then(|| {
            let joined = pushes.join(&MODEL_SEPARATOR.to_string());
            format!("{}\t{}\t{joined}\t{path}\n", stamp.0, stamp.1)
        })
    })();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(line), Some(file)) = (line, cache.file.clone()) {
        // One write per line, appended: runs in parallel do not interleave.
        let written = file
            .parent()
            .is_some_and(|dir| std::fs::create_dir_all(dir).is_ok())
            && std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file)
                .and_then(|mut f| f.write_all(line.as_bytes()))
                .is_ok();
        if !written {
            cache.file = None;
        }
    }
    cache.entries.insert(path.to_path_buf(), (stamp, pushes));
    models.non_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_rebuild_the_list() {
        for order in [&["a"][..], &["a", "b"], &["a", "b", "a"], &["a", "a"]] {
            let mut models = ModelList::default();
            order.iter().for_each(|m| models.push(m));
            let mut rebuilt = ModelList::default();
            pushes(&models).iter().for_each(|m| rebuilt.push(m));
            assert_eq!(
                (rebuilt.joined(), rebuilt.last().to_string()),
                (models.joined(), models.last().to_string()),
                "{order:?}"
            );
        }
    }
}

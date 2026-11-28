//! Models of compressed Codex rollouts, kept on disk between runs.
//!
//! Finding the models of a rollout reads all of it, which for a compressed
//! one means decoding it. Codex does not append to a compressed rollout (it
//! decompresses it first), so its models stay the same while its size and
//! mtime do. Each line of the cache file is
//! `<size>\t<mtime ns>\t<model>\x1f<model>...\t<path>`; lines are only
//! appended, the last line for a path wins, and lines that cannot be read
//! are ignored.
//!
//! The cache is only a shortcut, and is not trusted further than that: it
//! lives in a directory only the user can write to (else it is not used),
//! is never followed through a symbolic link, and is read up to a size.
//! A file past that size is started anew.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::UNIX_EPOCH;

use super::common::ModelList;

/// Separates the models of one line (models are never control characters).
const MODEL_SEPARATOR: char = '\x1f';
/// Largest cache file that is read; a larger one is started anew. A line
/// is about 200 bytes, so this holds tens of thousands of rollouts.
const MAX_CACHE_BYTES: u64 = 8 << 20;
/// Longest line that is read.
const MAX_LINE_BYTES: usize = 16 << 10;

/// Size and mtime (nanoseconds since the epoch) of a rollout.
type Stamp = (u64, u128);

struct Cache {
    /// The cache file; `None` once it cannot be used.
    file: Option<PathBuf>,
    /// Whether the file is started anew at the next write.
    truncate: bool,
    entries: HashMap<PathBuf, (Stamp, Vec<String>)>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
    // Unit tests do not touch the user's cache.
    let file = dirs::cache_dir()
        .filter(|_| !cfg!(test))
        .and_then(|dir| private_cache_dir(&dir.join("ah")))
        .map(|dir| dir.join("codex-zst-models-v1.tsv"));
    let mut cache = Cache {
        file,
        truncate: false,
        entries: HashMap::new(),
    };
    if let Some(file) = cache.file.clone() {
        match read_entries(&file) {
            Ok(entries) => cache.entries = entries,
            Err(Unreadable::TooLarge) => cache.truncate = true,
            Err(Unreadable::Unsafe) => cache.file = None,
        }
    }
    Mutex::new(cache)
});

/// `dir`, created only the user can use if missing; `None` unless it is a
/// directory of the user that others cannot write to.
#[cfg(unix)]
fn private_cache_dir(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    crate::cloud::private_dir(dir).then(|| dir.to_path_buf())
}

#[cfg(not(unix))]
fn private_cache_dir(_: &Path) -> Option<PathBuf> {
    None
}

/// The cache file opened without following a symbolic link or blocking on
/// a FIFO; `None` unless it is a regular file.
#[cfg(unix)]
fn open(file: &Path, options: &mut OpenOptions) -> std::io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file)?;
    Ok(f.metadata()?.is_file().then_some(f))
}

#[cfg(not(unix))]
fn open(_: &Path, _: &mut OpenOptions) -> std::io::Result<Option<File>> {
    Ok(None)
}

enum Unreadable {
    /// Larger than `MAX_CACHE_BYTES`: started anew.
    TooLarge,
    /// Not a regular file that can be opened: not used.
    Unsafe,
}

fn read_entries(file: &Path) -> Result<HashMap<PathBuf, (Stamp, Vec<String>)>, Unreadable> {
    let mut entries = HashMap::new();
    let f = match open(file, OpenOptions::new().read(true)) {
        Ok(Some(f)) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        _ => return Err(Unreadable::Unsafe),
    };
    if f.metadata().map_err(|_| Unreadable::Unsafe)?.len() > MAX_CACHE_BYTES {
        return Err(Unreadable::TooLarge);
    }
    let mut reader = BufReader::new(f.take(MAX_CACHE_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE_BYTES {
            // Skip the rest of a line too long to be one of ours.
            let mut rest = Vec::new();
            if !line.ends_with(b"\n") && reader.read_until(b'\n', &mut rest).is_err() {
                break;
            }
            continue;
        }
        if let Some((path, entry)) = std::str::from_utf8(&line).ok().and_then(parse_line) {
            entries.insert(path, entry);
        }
    }
    Ok(entries)
}

fn parse_line(line: &str) -> Option<(PathBuf, (Stamp, Vec<String>))> {
    let line = line.strip_suffix('\n')?;
    let mut fields = line.splitn(4, '\t');
    let size = fields.next()?.parse().ok()?;
    let mtime = fields.next()?.parse().ok()?;
    let models = fields
        .next()?
        .split(MODEL_SEPARATOR)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect();
    let path = fields.next().filter(|p| !p.is_empty())?;
    Some((PathBuf::from(path), ((size, mtime), models)))
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

fn rebuild(pushes: &[String]) -> Option<ModelList> {
    let mut models = ModelList::default();
    for model in pushes {
        models.push(model);
    }
    models.non_empty()
}

/// The models of the compressed rollout at `path`: from the cache while its
/// size and mtime are unchanged, else from `scan`, which also tells whether
/// it read the whole rollout. Only a whole read is kept.
pub(super) fn models(path: &Path, scan: impl FnOnce() -> (ModelList, bool)) -> Option<ModelList> {
    let Some(stamp) = stamp(path) else {
        return scan().0.non_empty();
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
    let (models, complete) = scan();
    // A rollout changed while it was read is read anew next time.
    if !complete || self::stamp(path) != Some(stamp) {
        return models.non_empty();
    }
    let pushes = pushes(&models);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let line = entry_line(path, stamp, &pushes);
    if let (Some(line), Some(file)) = (line, cache.file.clone()) {
        let mut options = OpenOptions::new();
        options.create(true);
        if cache.truncate {
            options.write(true).truncate(true);
        } else {
            options.append(true);
        }
        // One write per line, appended: runs in parallel do not interleave.
        let written = match open(&file, &mut options) {
            Ok(Some(mut f)) => f.write_all(line.as_bytes()).is_ok(),
            _ => false,
        };
        if written {
            cache.truncate = false;
        } else {
            cache.file = None;
        }
    }
    cache.entries.insert(path.to_path_buf(), (stamp, pushes));
    models.non_empty()
}

/// The cache line of a rollout; `None` when a field holds a separator.
fn entry_line(path: &Path, stamp: Stamp, pushes: &[String]) -> Option<String> {
    let path = path.to_str()?;
    let fine = |s: &str| !s.contains(['\t', '\n', '\r', MODEL_SEPARATOR]);
    (fine(path) && pushes.iter().all(|m| fine(m))).then(|| {
        let joined = pushes.join(&MODEL_SEPARATOR.to_string());
        format!("{}\t{}\t{joined}\t{path}\n", stamp.0, stamp.1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip_and_bad_lines_are_ignored() {
        let pushes = vec!["a".to_string(), "b".to_string()];
        let line = entry_line(Path::new("/r/x.jsonl.zst"), (3, 4), &pushes).unwrap();
        assert_eq!(
            parse_line(&line),
            Some((PathBuf::from("/r/x.jsonl.zst"), ((3, 4), pushes.clone())))
        );
        assert_eq!(entry_line(Path::new("/r/a\tb"), (3, 4), &pushes), None);
        for bad in ["", "3\t4\ta\t/p", "x\t4\ta\t/p\n", "3\t4\ta\t\n", "3\t4\n"] {
            assert_eq!(parse_line(bad), None, "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_cache_file_is_not_followed_through_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, "x").unwrap();
        let link = tmp.path().join("cache.tsv");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open(&link, OpenOptions::new().append(true)).is_err());
        assert!(matches!(read_entries(&link), Err(Unreadable::Unsafe)));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        // A file too large is started anew; a missing one is empty.
        let big = tmp.path().join("big.tsv");
        std::fs::write(&big, vec![b'x'; MAX_CACHE_BYTES as usize + 1]).unwrap();
        assert!(matches!(read_entries(&big), Err(Unreadable::TooLarge)));
        assert!(read_entries(&tmp.path().join("none")).is_ok_and(|e| e.is_empty()));
    }

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

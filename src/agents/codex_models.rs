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
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::UNIX_EPOCH;

use super::common::ModelList;

/// Separates the models of one line (models are never control characters).
const MODEL_SEPARATOR: char = '\x1f';
/// Largest cache file: a larger one is not read, and is started anew when
/// it would grow past this. A line is about 200 bytes, so this holds tens
/// of thousands of rollouts.
const MAX_CACHE_BYTES: u64 = 8 << 20;
/// Longest line that is read or written.
const MAX_LINE_BYTES: usize = 16 << 10;
const CACHE_FILE: &str = "codex-zst-models-v1.tsv";

/// Size and mtime (nanoseconds since the epoch) of a rollout.
type Stamp = (u64, u128);

struct Cache {
    /// The cache directory, opened once it was found private; `None` when
    /// the cache cannot be used. The file is opened relative to it, so
    /// that the directory cannot be swapped for another afterwards.
    dir: Option<File>,
    /// Whether the file is started anew at the next write.
    truncate: bool,
    entries: HashMap<PathBuf, (Stamp, Vec<String>)>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
    // Unit tests do not touch the user's cache.
    let dir = dirs::cache_dir()
        .filter(|_| !cfg!(test))
        .and_then(|dir| private_cache_dir(&dir.join("ah")));
    let mut cache = Cache {
        dir,
        truncate: false,
        entries: HashMap::new(),
    };
    if let Some(dir) = &cache.dir {
        match read_entries(dir) {
            Ok((entries, too_large)) => {
                cache.entries = entries;
                cache.truncate = too_large;
            }
            Err(Unsafe) => cache.dir = None,
        }
    }
    Mutex::new(cache)
});

/// `dir` opened, created only the user can use if missing; `None` unless
/// it is a directory (not a link to one) of the user that no one else can
/// write to.
#[cfg(unix)]
fn private_cache_dir(dir: &Path) -> Option<File> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)
        .ok()?;
    let meta = opened.metadata().ok()?;
    (meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0)
        .then_some(opened)
}

#[cfg(not(unix))]
fn private_cache_dir(_: &Path) -> Option<File> {
    None
}

/// The cache file in `dir`, opened with `flags` without following a
/// symbolic link or blocking on a FIFO; `None` unless it is a regular file.
#[cfg(unix)]
fn open_in(dir: &File, flags: libc::c_int) -> std::io::Result<Option<File>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(CACHE_FILE).expect("no NUL in the name");
    let mut attempts = 0;
    let fd = loop {
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd >= 0 {
            break fd;
        }
        let error = std::io::Error::last_os_error();
        // macOS can fail to create a file that another run is creating at
        // the same moment with ENOENT; the file is there on a retry.
        attempts += 1;
        if flags & libc::O_CREAT == 0
            || error.kind() != std::io::ErrorKind::NotFound
            || attempts >= 5
        {
            return Err(error);
        }
    };
    let file = unsafe { File::from_raw_fd(fd) };
    Ok(file.metadata()?.is_file().then_some(file))
}

#[cfg(not(unix))]
fn open_in(_: &File, _: i32) -> std::io::Result<Option<File>> {
    Ok(None)
}

#[cfg(unix)]
const READ: libc::c_int = libc::O_RDONLY;
#[cfg(unix)]
const APPEND: libc::c_int = libc::O_RDWR | libc::O_CREAT | libc::O_APPEND;
#[cfg(not(unix))]
const READ: i32 = 0;
#[cfg(not(unix))]
const APPEND: i32 = 0;

/// The cache file is not a regular file that can be opened: not used.
struct Unsafe;

type Entries = HashMap<PathBuf, (Stamp, Vec<String>)>;

/// The entries of the cache file in `dir`, read up to `MAX_CACHE_BYTES`,
/// and whether the file is larger than that (it is then written anew).
fn read_entries(dir: &File) -> Result<(Entries, bool), Unsafe> {
    match open_in(dir, READ) {
        Ok(Some(f)) => entries_of(&f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((HashMap::new(), false)),
        _ => Err(Unsafe),
    }
}

/// The entries of the open cache file `f`, read from its start.
fn entries_of(f: &File) -> Result<(Entries, bool), Unsafe> {
    let mut entries = HashMap::new();
    let too_large = f.metadata().map_err(|_| Unsafe)?.len() > MAX_CACHE_BYTES;
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
    Ok((entries, too_large))
}

/// How an append went.
#[derive(PartialEq, Debug)]
enum Appended {
    Written,
    /// Another run held the file for too long: this entry is not written.
    Busy,
    /// The file cannot be written: the cache is not used further.
    Failed,
}

/// Append `line`, the entry of `path`, to the cache file in `dir`, one
/// write per line. When `rewrite`, or when the line would take the file
/// past `MAX_CACHE_BYTES`, the file is written anew instead, with one line
/// per path (those of `entries`, then the others in the file, then `line`)
/// as many as fit. The file is locked meanwhile, so that runs in parallel
/// do not rewrite it over each other.
fn append(dir: &File, path: &Path, line: &str, rewrite: bool, entries: &Entries) -> Appended {
    let Ok(Some(mut f)) = open_in(dir, APPEND) else {
        return Appended::Failed;
    };
    match lock(&f) {
        Some(true) => {}
        Some(false) => return Appended::Busy,
        None => return Appended::Failed,
    }
    let full = f
        .metadata()
        .is_ok_and(|m| m.len() + line.len() as u64 > MAX_CACHE_BYTES);
    if !rewrite && !full {
        return written(f.write_all(line.as_bytes()));
    }
    // What other runs wrote since this one read the file is kept too.
    let in_file = entries_of(&f)
        .map(|(entries, _)| entries)
        .unwrap_or_default();
    let others = in_file.iter().filter(|(p, _)| !entries.contains_key(*p));
    let mut text = String::new();
    for (p, (stamp, pushes)) in entries.iter().chain(others) {
        if p == path {
            continue;
        }
        if let Some(entry) = entry_line(p, *stamp, pushes)
            && (text.len() + entry.len() + line.len()) as u64 <= MAX_CACHE_BYTES
        {
            text.push_str(&entry);
        }
    }
    text.push_str(line);
    if f.set_len(0).is_err() {
        return Appended::Failed;
    }
    written(f.write_all(text.as_bytes()))
}

fn written(result: std::io::Result<()>) -> Appended {
    if result.is_ok() {
        Appended::Written
    } else {
        Appended::Failed
    }
}

/// Lock `f` for this process until it is closed. `Some(false)` when
/// another run holds it past `LOCK_WAIT`, or the command is cancelled.
#[cfg(unix)]
fn lock(f: &File) -> Option<bool> {
    use std::os::fd::AsRawFd;
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Some(true);
        }
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) {
            return None;
        }
        if crate::cancel::requested() || std::time::Instant::now() >= deadline {
            return Some(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(not(unix))]
fn lock(_: &File) -> Option<bool> {
    None
}

/// How long a run waits for another to finish writing the cache.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

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
    // Rewriting the file writes the new entry, not the one it replaces.
    cache.entries.remove(path);
    if let (Some(line), Some(dir)) = (line, &cache.dir) {
        match append(dir, path, &line, cache.truncate, &cache.entries) {
            Appended::Written => cache.truncate = false,
            Appended::Busy => {}
            Appended::Failed => cache.dir = None,
        }
    }
    cache.entries.insert(path.to_path_buf(), (stamp, pushes));
    models.non_empty()
}

/// The cache line of a rollout; `None` when a field holds a separator.
fn entry_line(path: &Path, stamp: Stamp, pushes: &[String]) -> Option<String> {
    let path = path.to_str()?;
    let fine = |s: &str| !s.contains(['\t', '\n', '\r', MODEL_SEPARATOR]);
    (fine(path) && pushes.iter().all(|m| fine(m)))
        .then(|| {
            let joined = pushes.join(&MODEL_SEPARATOR.to_string());
            format!("{}\t{}\t{joined}\t{path}\n", stamp.0, stamp.1)
        })
        .filter(|line| line.len() <= MAX_LINE_BYTES)
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
    fn the_cache_file_is_opened_safely_and_kept_small() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_path = tmp.path().join("ah");
        let dir = private_cache_dir(&dir_path).unwrap();
        let file = dir_path.join(CACHE_FILE);
        assert!(read_entries(&dir).is_ok_and(|(e, too_large)| e.is_empty() && !too_large));
        // A link is not followed, for reading or writing.
        let target = tmp.path().join("target");
        std::fs::write(&target, "x").unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert!(read_entries(&dir).is_err());
        assert_eq!(
            append(&dir, Path::new("/r/new"), "line\n", false, &HashMap::new()),
            Appended::Failed
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        std::fs::remove_file(&file).unwrap();
        // A file too large is read up to the limit, and written anew with
        // the entries known, one per path.
        let kept = entry_line(Path::new("/r/kept"), (1, 2), &["m".to_string()]).unwrap();
        let mut big = kept.repeat(2).into_bytes();
        big.resize(MAX_CACHE_BYTES as usize + 1, b'x');
        std::fs::write(&file, big).unwrap();
        let (entries, too_large) = read_entries(&dir).ok().unwrap();
        assert!(too_large);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            append(&dir, Path::new("/r/new"), "line\n", true, &entries),
            Appended::Written
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{kept}line\n")
        );
        // A rewrite keeps the entries other runs wrote, one per path, and
        // drops the old entry of the path written.
        let other = entry_line(Path::new("/r/other"), (3, 4), &["o".to_string()]).unwrap();
        let old = entry_line(Path::new("/r/new"), (5, 6), &["x".to_string()]).unwrap();
        std::fs::write(&file, format!("{other}{old}{other}")).unwrap();
        let new = entry_line(Path::new("/r/new"), (7, 8), &["y".to_string()]).unwrap();
        assert_eq!(
            append(&dir, Path::new("/r/new"), &new, true, &entries),
            Appended::Written
        );
        let text = std::fs::read_to_string(&file).unwrap();
        let mut lines: Vec<_> = text.lines().collect();
        lines.sort();
        let mut expected = vec![kept.trim_end(), other.trim_end(), new.trim_end()];
        expected.sort();
        assert_eq!(lines, expected);
        // So is a file the line would take past the limit.
        std::fs::write(&file, vec![b'x'; MAX_CACHE_BYTES as usize - 2]).unwrap();
        assert_eq!(
            append(&dir, Path::new("/r/new"), "line\n", false, &entries),
            Appended::Written
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{kept}line\n")
        );
        // A directory others can write to, or a link to one, is not used.
        let shared = tmp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(private_cache_dir(&shared).is_none());
        let linked = tmp.path().join("linked");
        std::os::unix::fs::symlink(&dir_path, &linked).unwrap();
        assert!(private_cache_dir(&linked).is_none());
        // A line too long is not written.
        let long = "m".repeat(MAX_LINE_BYTES);
        assert_eq!(entry_line(Path::new("/r/x"), (1, 2), &[long]), None);
    }

    #[cfg(unix)]
    #[test]
    fn rewrites_in_parallel_keep_every_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_path = tmp.path().join("ah");
        private_cache_dir(&dir_path).unwrap();
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let dir_path = dir_path.clone();
                std::thread::spawn(move || {
                    // Each run holds the directory open as `CACHE` does.
                    let dir = private_cache_dir(&dir_path).unwrap();
                    let path = PathBuf::from(format!("/r/{i}"));
                    let line = entry_line(&path, (i, 0), &["m".to_string()]).unwrap();
                    assert_eq!(
                        append(&dir, &path, &line, true, &HashMap::new()),
                        Appended::Written
                    );
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let dir = private_cache_dir(&dir_path).unwrap();
        let (entries, too_large) = read_entries(&dir).ok().unwrap();
        assert!(!too_large);
        assert_eq!(entries.len(), 16);
        let text = std::fs::read_to_string(dir_path.join(CACHE_FILE)).unwrap();
        assert_eq!(text.lines().count(), 16);
        // A run that holds the file past the wait makes others skip it.
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(dir_path.join(CACHE_FILE)).unwrap();
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        let line = entry_line(Path::new("/r/late"), (1, 0), &["m".to_string()]).unwrap();
        let path = Path::new("/r/late");
        assert_eq!(
            append(&dir, path, &line, false, &HashMap::new()),
            Appended::Busy
        );
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

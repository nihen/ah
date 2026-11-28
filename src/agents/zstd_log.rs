//! Session logs stored as Zstandard: a concatenation of frames whose
//! decoded text is the JSONL log. Shared by the plugins whose agents
//! compress their logs (DeepSeek Harness, and Codex rollouts once
//! compressed).

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use super::common::{SessionBytes, mmap_file};

/// Largest decoded log kept for the next field of the same session. Larger
/// ones are freed once read, so idle threads (e.g. of `ah mcp`) hold little.
const MAX_CACHED_BYTES: usize = 8 << 20;
/// Whole logs decoded at the same time, across threads: with the decode
/// limit of each plugin this bounds the text being decoded at once.
const MAX_PARALLEL_DECODES: usize = 4;

/// Holds one of the `MAX_PARALLEL_DECODES` slots while alive.
struct DecodeSlot;

static DECODES: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
static DECODE_FREED: std::sync::Condvar = std::sync::Condvar::new();

impl DecodeSlot {
    fn acquire() -> Self {
        let mut busy = DECODES.lock().unwrap_or_else(|e| e.into_inner());
        while *busy >= MAX_PARALLEL_DECODES {
            busy = DECODE_FREED.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        *busy += 1;
        DecodeSlot
    }
}

impl Drop for DecodeSlot {
    fn drop(&mut self) {
        *DECODES.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        DECODE_FREED.notify_one();
    }
}

/// Decode the Zstandard frames of `data` one at a time, up to `max_bytes`
/// of text in all, until `visit` returns `false`. A frame that cannot be
/// decoded ends the log, and only the frames before it are visited: a
/// write that was cut short, a checksum that does not match, or a frame
/// that would take the text past `max_bytes`.
pub(super) fn for_each_frame(
    mut data: &[u8],
    max_bytes: u64,
    mut visit: impl FnMut(Vec<u8>) -> bool,
) {
    let mut room = max_bytes;
    while !data.is_empty() {
        let mut text = Vec::new();
        let decoded = zstd::stream::read::Decoder::with_buffer(&mut data)
            .ok()
            .and_then(|frame| {
                frame
                    .single_frame()
                    .take(room + 1)
                    .read_to_end(&mut text)
                    .ok()
            });
        if decoded.is_none_or(|read| read as u64 > room) {
            return;
        }
        room -= text.len() as u64;
        if !visit(text) {
            return;
        }
    }
}

/// The text of the first `max_frames` frames of `data` (see `for_each_frame`).
pub(super) fn decode_frames(data: &[u8], max_frames: usize, max_bytes: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut frames = 0;
    for_each_frame(data, max_bytes, |text| {
        if out.is_empty() {
            out = text;
        } else {
            out.extend_from_slice(&text);
        }
        frames += 1;
        frames < max_frames
    });
    out
}

/// The first line of the decoded text, without its newline, decoding no
/// more than that line. `None` when the line is longer than `max_bytes` or
/// the data cannot be decoded before its end.
pub(super) fn first_line(data: &[u8], max_bytes: u64) -> Option<Vec<u8>> {
    let decoder = zstd::stream::read::Decoder::with_buffer(data).ok()?;
    let mut line = Vec::new();
    BufReader::new(decoder.take(max_bytes + 1))
        .read_until(b'\n', &mut line)
        .ok()?;
    if line.last() == Some(&b'\n') {
        line.pop();
    } else if line.len() as u64 > max_bytes {
        return None;
    }
    Some(line)
}

/// A log as text: the file itself, or its decoded frames.
pub(super) enum Log {
    Plain(memmap2::Mmap),
    Decoded(Rc<Vec<u8>>),
}

impl std::ops::Deref for Log {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Log::Plain(mmap) => mmap,
            Log::Decoded(bytes) => bytes,
        }
    }
}

impl Log {
    /// The log as session bytes. Decoded bytes are handed over rather than
    /// copied when nothing else holds them.
    pub(super) fn into_session_bytes(self) -> SessionBytes {
        match self {
            Log::Plain(mmap) => SessionBytes::Mmap(mmap),
            Log::Decoded(bytes) => {
                LAST_DECODED.with_borrow_mut(|last| *last = None);
                SessionBytes::Owned(Rc::try_unwrap(bytes).unwrap_or_else(|shared| shared.to_vec()))
            }
        }
    }
}

/// The file a decoded log was read from, to tell when it has changed.
#[derive(PartialEq)]
struct Source {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    epoch: u64,
}

/// Raised by `clear_cache`; a decoded log from an earlier epoch is stale.
static EPOCH: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The log decoded last on this thread. The fields of one session are
    /// resolved one after another, each asking for the same log.
    static LAST_DECODED: RefCell<Option<(Source, Rc<Vec<u8>>)>> = const { RefCell::new(None) };
}

/// Forget the decoded logs, e.g. when sessions are collected again.
pub(super) fn clear_cache() {
    EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// The log at `path`: mapped as it is, or, when `compressed`, decoded up
/// to `max_bytes` of text.
pub(super) fn read_log(path: &Path, compressed: bool, max_bytes: u64) -> Option<Log> {
    if !compressed {
        return mmap_file(path).map(Log::Plain);
    }
    let meta = std::fs::metadata(path).ok()?;
    let source = Source {
        path: path.to_path_buf(),
        len: meta.len(),
        modified: meta.modified().ok(),
        epoch: EPOCH.load(Ordering::Relaxed),
    };
    let cached = LAST_DECODED.with_borrow(|last| {
        last.as_ref()
            .filter(|(of, _)| *of == source)
            .map(|(_, bytes)| Rc::clone(bytes))
    });
    if let Some(bytes) = cached {
        return Some(Log::Decoded(bytes));
    }
    let data = mmap_file(path)?;
    let slot = DecodeSlot::acquire();
    let bytes = Rc::new(decode_frames(&data, usize::MAX, max_bytes));
    drop(slot);
    LAST_DECODED.with_borrow_mut(|last| {
        *last = (bytes.len() <= MAX_CACHED_BYTES).then(|| (source, Rc::clone(&bytes)));
    });
    Some(Log::Decoded(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Vec<u8> {
        zstd::stream::encode_all(text.as_bytes(), 3).unwrap()
    }

    #[test]
    fn first_line_decodes_only_the_first_line() {
        let data = frame("{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(first_line(&data, 64).as_deref(), Some(&b"{\"a\":1}"[..]));
        // A log of one line without a newline.
        assert_eq!(
            first_line(&frame("{\"a\":1}"), 64).as_deref(),
            Some(&b"{\"a\":1}"[..])
        );
        // Exactly at the limit, and one byte past it.
        assert_eq!(first_line(&data, 7).as_deref(), Some(&b"{\"a\":1}"[..]));
        assert_eq!(first_line(&frame("{\"a\":1}"), 6), None);
        assert_eq!(first_line(b"not zstd", 64), None);
    }
}

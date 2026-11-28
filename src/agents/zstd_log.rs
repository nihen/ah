//! Session logs stored as Zstandard: a concatenation of frames whose
//! decoded text is the JSONL log. Shared by the plugins whose agents
//! compress their logs (DeepSeek Harness, and Codex rollouts once
//! compressed).

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
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

/// Largest window a frame may ask the decoder for, as a power of two
/// (16 MiB). Codex and dsh write frames with windows of 2 MiB or less; a
/// frame asking for more is not decoded, so that a hostile header cannot
/// make each decoder allocate the library's maximum.
const WINDOW_LOG_MAX: u32 = 24;
/// A line read line by line that grows past this many bytes takes one of
/// the `MAX_PARALLEL_LONG_LINES` slots until it has been visited, which
/// bounds the long lines held at once across threads.
const LONG_LINE_BYTES: usize = 16 << 20;
const MAX_PARALLEL_LONG_LINES: usize = 2;
/// A line freed after it was visited when its buffer grew past this, so a
/// thread does not keep the capacity of its longest line.
const KEPT_LINE_CAPACITY: usize = 1 << 20;

/// A decoder of the frames read from `input` whose window is at most
/// `WINDOW_LOG_MAX`.
fn decoder<R: BufRead>(input: R) -> Option<zstd::stream::read::Decoder<'static, R>> {
    let mut decoder = zstd::stream::read::Decoder::with_buffer(input).ok()?;
    decoder.window_log_max(WINDOW_LOG_MAX).ok()?;
    Some(decoder)
}

/// Counted slots: at most `max` holders at once, across threads.
struct Slots {
    busy: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
    max: usize,
}

impl Slots {
    const fn new(max: usize) -> Self {
        Slots {
            busy: std::sync::Mutex::new(0),
            freed: std::sync::Condvar::new(),
            max,
        }
    }

    fn acquire(&'static self) -> Slot {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        while *busy >= self.max {
            busy = self.freed.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        *busy += 1;
        Slot(self)
    }
}

/// One slot of `Slots`, released when dropped.
struct Slot(&'static Slots);

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.freed.notify_one();
    }
}

static LONG_LINES: Slots = Slots::new(MAX_PARALLEL_LONG_LINES);

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
            .and_then(|mut frame| frame.window_log_max(WINDOW_LOG_MAX).ok().map(|()| frame))
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
pub(super) fn first_line(input: impl BufRead, max_bytes: u64) -> Option<Vec<u8>> {
    let decoder = decoder(input)?;
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

/// The compressed file at `path`, for the readers above. Read rather than
/// mapped: the readers pass over it once, and its pages are then not
/// counted against the process.
pub(super) fn open(path: &Path) -> std::io::Result<BufReader<std::fs::File>> {
    Ok(BufReader::with_capacity(
        1 << 16,
        std::fs::File::open(path)?,
    ))
}

/// Visit the lines of the decoded text until `visit` returns `false`,
/// decoding them one at a time: only one line and the decoder's window are
/// held, however large the log is. Each line is visited with its newline,
/// when it has one. Decoding stops after `max_bytes` of text, at data that
/// cannot be decoded (the lines before it are visited), and when the
/// command is cancelled. A line longer than `max_line` (without its
/// newline) is skipped. Returns whether the text was read to its end
/// (or to `max_bytes`) with every line visited.
pub(super) fn for_each_line(
    input: impl BufRead,
    max_bytes: u64,
    max_line: usize,
    mut visit: impl FnMut(&[u8]) -> bool,
) -> bool {
    let Some(decoder) = decoder(input) else {
        return false;
    };
    let mut reader = BufReader::with_capacity(1 << 16, decoder.take(max_bytes));
    let mut line = Vec::new();
    let mut complete = true;
    loop {
        line.clear();
        let mut long_line = None;
        let mut skipped = false;
        loop {
            if crate::cancel::requested() {
                return false;
            }
            let Ok(buf) = reader.fill_buf() else {
                return false;
            };
            if buf.is_empty() {
                if line.is_empty() && !skipped {
                    return complete;
                }
                break;
            }
            let (chunk, done) = match memchr::memchr(b'\n', buf) {
                Some(end) => (&buf[..end], Some(end + 1)),
                None => (buf, None),
            };
            if skipped {
                // Dropping the rest of a line past `max_line`.
            } else if line.len() + chunk.len() <= max_line {
                if long_line.is_none() && line.len() + chunk.len() > LONG_LINE_BYTES {
                    long_line = Some(LONG_LINES.acquire());
                }
                line.extend_from_slice(chunk);
                if done.is_some() {
                    line.push(b'\n');
                }
            } else {
                skipped = true;
                complete = false;
                line = Vec::new();
                long_line = None;
            }
            let used = done.unwrap_or(buf.len());
            reader.consume(used);
            if done.is_some() {
                break;
            }
        }
        if !skipped && !visit(&line) {
            return false;
        }
        drop(long_line);
        if line.capacity() > KEPT_LINE_CAPACITY {
            line = Vec::new();
        }
    }
}

/// A reader that fails once the command is cancelled.
struct Cancellable<R>(R);

impl<R: Read> Read for Cancellable<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if crate::cancel::requested() {
            return Err(std::io::Error::other("cancelled"));
        }
        self.0.read(buf)
    }
}

/// Write the decoded text to `out` as it is, up to `max_bytes`. Decoding
/// stops at data that cannot be decoded, and when the command is
/// cancelled, which are errors.
pub(super) fn copy_to(
    input: impl BufRead,
    max_bytes: u64,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let decoder = decoder(input).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "not a Zstandard stream")
    })?;
    std::io::copy(&mut Cancellable(decoder.take(max_bytes)), out)?;
    Ok(())
}

/// Whole logs held decoded at the same time, across threads (see
/// `decode_whole`).
static WHOLE_LOGS: Slots = Slots::new(1);

thread_local! {
    /// Whole logs this thread holds: a thread that holds one takes another
    /// without waiting, so that it cannot wait for itself.
    static HELD_WHOLE_LOGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A slot of `WHOLE_LOGS` held with the text decoded in it.
struct WholeLogSlot {
    _slot: Option<Slot>,
}

impl WholeLogSlot {
    fn acquire() -> Self {
        let held = HELD_WHOLE_LOGS.get();
        HELD_WHOLE_LOGS.set(held + 1);
        WholeLogSlot {
            _slot: (held == 0).then(|| WHOLE_LOGS.acquire()),
        }
    }
}

impl Drop for WholeLogSlot {
    fn drop(&mut self) {
        HELD_WHOLE_LOGS.set(HELD_WHOLE_LOGS.get().saturating_sub(1));
    }
}

/// The whole decoded text of the log at `path`, up to `max_bytes`, as
/// session bytes that hold one of the `WHOLE_LOGS` slots for as long as
/// they live: only one thread holds a whole log at a time. `None` when the
/// file cannot be read or decoded to its end, when the text is longer than
/// `max_bytes`, or when the command is cancelled.
pub(super) fn decode_whole(path: &Path, max_bytes: u64) -> Option<SessionBytes> {
    let input = open(path).ok()?;
    let slot = WholeLogSlot::acquire();
    let mut text = Vec::new();
    let read = Cancellable(decoder(input)?)
        .take(max_bytes + 1)
        .read_to_end(&mut text)
        .ok()?;
    if read as u64 > max_bytes {
        return None;
    }
    Some(SessionBytes::Guarded {
        bytes: text,
        _guard: Box::new(slot),
    })
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
        assert_eq!(
            first_line(&data[..], 64).as_deref(),
            Some(&b"{\"a\":1}"[..])
        );
        // A log of one line without a newline.
        assert_eq!(
            first_line(&frame("{\"a\":1}")[..], 64).as_deref(),
            Some(&b"{\"a\":1}"[..])
        );
        // Exactly at the limit, and one byte past it.
        assert_eq!(first_line(&data[..], 7).as_deref(), Some(&b"{\"a\":1}"[..]));
        assert_eq!(first_line(&frame("{\"a\":1}")[..], 6), None);
        assert_eq!(first_line(&b"not zstd"[..], 64), None);
    }

    fn lines(data: &[u8], max_bytes: u64, max_line: usize) -> Vec<String> {
        let mut seen = Vec::new();
        for_each_line(data, max_bytes, max_line, |line| {
            seen.push(String::from_utf8_lossy(line).into_owned());
            true
        });
        seen
    }

    #[test]
    fn lines_are_decoded_one_at_a_time() {
        // Two frames, the second without a trailing newline.
        let data = [frame("a\nlong line\n"), frame("\nb")].concat();
        assert_eq!(lines(&data, 1 << 20, 64), ["a\n", "long line\n", "\n", "b"]);
        // A line past `max_line` is skipped, not cut, and reading goes on.
        assert_eq!(lines(&data, 1 << 20, 5), ["a\n", "\n", "b"]);
        let long = frame(&format!("a\n{}\nb\n", "x".repeat(200)));
        assert_eq!(lines(&long, 1 << 20, 9), ["a\n", "b\n"]);
        // Decoding stops after `max_bytes` of text.
        assert_eq!(lines(&data, 4, 64), ["a\n", "lo"]);
        // A torn frame ends the log after the lines before it.
        let torn = [frame("a\n"), frame("never\n")[..5].to_vec()].concat();
        assert_eq!(lines(&torn, 1 << 20, 64), ["a\n"]);
        assert!(lines(b"not zstd", 1 << 20, 64).is_empty());
        // Whether the text was read to its end with every line visited.
        let read_all = |data: &[u8], max_line| for_each_line(data, 1 << 20, max_line, |_| true);
        assert!(read_all(&data, 64));
        assert!(!read_all(&data, 5));
        assert!(!read_all(&torn, 64));
        assert!(!read_all(b"not zstd", 64));
        // `visit` stops the iteration.
        let mut count = 0;
        assert!(!for_each_line(&data[..], 1 << 20, 64, |_| {
            count += 1;
            false
        }));
        assert_eq!(count, 1);
    }

    #[test]
    fn copy_keeps_the_text_as_it_is() {
        let data = [frame("a\n"), frame("b")].concat();
        let mut out = Vec::new();
        copy_to(&data[..], 1 << 20, &mut out).unwrap();
        assert_eq!(out, b"a\nb");
        assert!(copy_to(&b"not zstd"[..], 1 << 20, &mut Vec::new()).is_err());
    }

    #[test]
    fn frames_asking_for_a_large_window_are_not_decoded() {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        encoder.window_log(WINDOW_LOG_MAX + 1).unwrap();
        encoder.write_all(&vec![b'x'; 64 << 20]).unwrap();
        encoder.write_all(b"\n").unwrap();
        let data = encoder.finish().unwrap();
        assert!(lines(&data, 1 << 30, 1 << 30).is_empty());
        assert!(copy_to(&data[..], 1 << 30, &mut Vec::new()).is_err());
    }
}

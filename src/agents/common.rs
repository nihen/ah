use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use memmap2::Mmap;
use regex::Regex;

/// Resolve and canonicalize the user's home directory.
pub fn canonical_home() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    fs::canonicalize(&home).unwrap_or(home)
}

pub fn format_mtime(mtime: SystemTime) -> String {
    let dt: DateTime<Local> = mtime.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// Strip the home directory prefix from `path` (path-boundary aware:
/// `/home/al` does not match `/home/alice/repo`).
pub fn strip_home(path: &str, home: &Path) -> String {
    let home_str = home.to_string_lossy();
    let home_str = home_str.trim_end_matches('/');
    if let Some(rest) = path.strip_prefix(home_str) {
        if rest.is_empty() || rest.starts_with('/') {
            return rest.trim_start_matches('/').to_string();
        }
    }
    path.to_string()
}

/// Canonicalize `path` if it exists on disk (resolves symlinks such as
/// `/home/user` → `/data/home/user` so that it matches the canonicalized cwd
/// filter); otherwise return it unchanged.
pub fn canonicalize_if_exists(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// A session/conversation id is safe to pass as a positional CLI argument
/// (must not be empty or look like an option).
pub fn is_safe_cli_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('-')
}

/// Regex to strip home-directory prefix from Claude project directory names.
/// Matches patterns like `-Users-you-` or `-home-user-`.
pub static RE_HOME_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-(Users|data-home|home)-[^-]+-").unwrap());

/// Decode a Claude-encoded project directory name to a project basename.
/// e.g. `-Users-you-src-github-com-org-myapp` → `myapp`
pub fn decode_claude_project(encoded_dir: &str) -> String {
    let full = RE_HOME_PREFIX.replace(encoded_dir, "").replace('-', "/");
    full.rsplit('/').next().unwrap_or(&full).to_string()
}

/// Raw session bytes used for full-text search: a memory-mapped file, or an
/// owned buffer for sessions that are not stored as a single file.
pub enum SessionBytes {
    Mmap(Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for SessionBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            SessionBytes::Mmap(m) => m,
            SessionBytes::Owned(v) => v,
        }
    }
}

pub fn mmap_file(path: &Path) -> Option<Mmap> {
    let file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if meta.len() == 0 {
        return None;
    }
    unsafe { Mmap::map(&file) }.ok()
}

pub fn for_each_jsonl_value(path: &Path, visit: impl FnMut(&serde_json::Value) -> bool) {
    let mmap = match mmap_file(path) {
        Some(mmap) => mmap,
        None => return,
    };
    for_each_jsonl_value_bytes(&mmap, visit);
}

pub fn for_each_jsonl_value_bytes(data: &[u8], mut visit: impl FnMut(&serde_json::Value) -> bool) {
    for line_bytes in data.split(|&b| b == b'\n') {
        if line_bytes.len() < 2 {
            continue;
        }
        if let Ok(line) = std::str::from_utf8(line_bytes) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if !visit(&val) {
                    return;
                }
            }
        }
    }
}

pub fn first_text_part(val: &serde_json::Value) -> Option<&str> {
    val.as_str()
        .or_else(|| val.pointer("/0/text").and_then(|v| v.as_str()))
        .or_else(|| val.pointer("/content/0/text").and_then(|v| v.as_str()))
        .or_else(|| {
            // e.g. Gemini `[{"text":"..."}]`, or Cursor `[{type:text,...},{tool_use,...}]`
            val.as_array().and_then(|arr| {
                arr.iter().find_map(|item| {
                    if item.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        return None;
                    }
                    item.get("text").and_then(|v| v.as_str())
                })
            })
        })
}

/// `type`s of content parts whose only text is `text`.
const TEXT_PART_TYPES: &[&str] = &["text", "input_text", "output_text"];

/// `type`s of content parts that carry encoded media rather than text.
const MEDIA_PART_TYPES: &[&str] = &["image", "input_image", "image_url", "document"];

/// Visit every string of `val` (e.g. a tool call's arguments, whose keys are
/// the tool's own and may be named anything). Numbers are not visited: their
/// parsed form (`1e3` → `1000.0`) need not appear in the raw bytes, which
/// the raw-bytes prefilters rely on. Returns `false` when `visit` stops.
pub fn visit_all_strings(val: &serde_json::Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match val {
        serde_json::Value::String(s) => visit(s),
        serde_json::Value::Array(items) => items.iter().all(|v| visit_all_strings(v, visit)),
        serde_json::Value::Object(map) => map.values().all(|v| visit_all_strings(v, visit)),
        _ => true,
    }
}

/// Keys that hold encoded data in any tool output object: Gemini media
/// parts and encrypted server-tool results.
const OPAQUE_OUTPUT_KEYS: &[&str] = &["inlineData", "encrypted_content"];

/// Visit the strings of a tool's output. Known content parts are reduced to
/// their text (`{"type":"text","text":...}`) or skipped when they carry
/// media (`{"type":"image","source":{...}}`); `OPAQUE_OUTPUT_KEYS` are
/// skipped; any other object is the tool's own data and is visited in full.
pub fn visit_tool_output(val: &serde_json::Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match val {
        serde_json::Value::String(s) => visit(s),
        serde_json::Value::Array(items) => items.iter().all(|v| visit_tool_output(v, visit)),
        serde_json::Value::Object(map) => {
            let kind = map.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if TEXT_PART_TYPES.contains(&kind) {
                return map.get("text").is_none_or(|v| visit_tool_output(v, visit));
            }
            if MEDIA_PART_TYPES.contains(&kind) {
                return true;
            }
            map.iter()
                .filter(|(key, _)| !OPAQUE_OUTPUT_KEYS.contains(&key.as_str()))
                .all(|(_, v)| visit_tool_output(v, visit))
        }
        _ => true,
    }
}

/// Visit a tool call's `name` and every string of its `input`.
pub fn visit_tool_call(
    name: Option<&serde_json::Value>,
    input: Option<&serde_json::Value>,
    visit: &mut dyn FnMut(&str) -> bool,
) -> bool {
    name.and_then(|v| v.as_str()).is_none_or(&mut *visit)
        && input.is_none_or(|v| visit_all_strings(v, visit))
}

/// Visit every text of a message body: a string, or the `text` of each part
/// of an array (or of `content` when `val` is a message object) except
/// `tool_use` parts. `first_text_part` returns only the first of these.
pub fn visit_text_parts(val: &serde_json::Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    if let Some(text) = val.as_str() {
        return visit(text);
    }
    let parts = val
        .as_array()
        .or_else(|| val.get("content").and_then(|v| v.as_array()));
    parts.is_none_or(|parts| {
        parts.iter().all(|part| {
            part.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                || part
                    .get("text")
                    .and_then(|v| v.as_str())
                    .is_none_or(&mut *visit)
        })
    }) && val
        .get("content")
        .and_then(|v| v.as_str())
        .is_none_or(&mut *visit)
}

/// Extract the body wrapped in `<tag>…</tag>` from a raw user message.
/// Several agents wrap the real user text in a tag (Cursor: `user_query`,
/// Grok: `user_query`, Antigravity: `USER_REQUEST`) and append metadata
/// blocks after it. If the tag is absent and the text does not look like
/// injected markup (i.e. does not start with `<`), the trimmed text is
/// returned as-is.
pub fn tagged_user_body<'a>(raw: &'a str, tag: &str) -> Option<&'a str> {
    let t = raw.trim();
    // Locate `<tag>` / `</tag>` without allocating: scan '<' positions and
    // compare the following bytes against the tag in place.
    let open_at = t
        .match_indices('<')
        .find(|(i, _)| {
            let rest = &t[i + 1..];
            rest.starts_with(tag) && rest[tag.len()..].starts_with('>')
        })
        .map(|(i, _)| i);
    let close_at = t
        .rmatch_indices("</")
        .find(|(i, _)| {
            let rest = &t[i + 2..];
            rest.starts_with(tag) && rest[tag.len()..].starts_with('>')
        })
        .map(|(i, _)| i);
    if let (Some(i), Some(j)) = (open_at, close_at) {
        let start = i + 1 + tag.len() + 1;
        if j >= start {
            let inner = t[start..j].trim();
            if !inner.is_empty() {
                return Some(inner);
            }
        }
    }
    if !t.starts_with('<') {
        return Some(t);
    }
    None
}

/// Decode a percent-encoded string (e.g. `%2Fdata%2Fhome` → `/data/home`).
/// Invalid escapes are passed through unchanged.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `pid` is a live process. A zombie (exited but not yet reaped by
/// its parent) still answers `kill(pid, 0)`, so on Linux its state is
/// checked too.
#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    let signalable = match i32::try_from(pid) {
        Ok(p) if p > 0 => unsafe { libc::kill(p, 0) == 0 },
        _ => false,
    };
    signalable && !is_zombie(pid)
}

/// `/proc/<pid>/stat`. The command name in it is not necessarily UTF-8 (it
/// can be cut mid-character), so it is decoded lossily.
#[cfg(target_os = "linux")]
fn read_proc_stat(pid: u32) -> Option<String> {
    fs::read(format!("/proc/{}/stat", pid))
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    read_proc_stat(pid)
        .and_then(|stat| parse_stat_state(&stat))
        .is_some_and(|state| matches!(state, 'Z' | 'X' | 'x'))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn is_zombie(_pid: u32) -> bool {
    false
}

/// Extract field 3 (state) from a `/proc/<pid>/stat` line.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat_state(stat: &str) -> Option<char> {
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .next()?
        .chars()
        .next()
}

#[cfg(not(unix))]
pub fn is_pid_alive(_pid: u32) -> bool {
    false
}

/// Parse the `u32` stored under `key` (JSON number or numeric string).
pub fn json_pid(val: &serde_json::Value, key: &str) -> Option<u32> {
    let v = val.get(key)?;
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))?;
    u32::try_from(n).ok().filter(|&p| p > 0)
}

/// Start time of `pid` in clock ticks since boot (`/proc/<pid>/stat` field
/// 22). `None` where the kernel does not expose it.
pub fn process_start_ticks(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        parse_stat_start_ticks(&read_proc_stat(pid)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Extract field 22 (starttime) from a `/proc/<pid>/stat` line. The command
/// name (field 2) may contain spaces and parentheses, so fields are counted
/// from the last `)`.
fn parse_stat_start_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Wall-clock start time of `pid`. `None` where it cannot be determined.
pub fn process_start_time(pid: u32) -> Option<SystemTime> {
    #[cfg(target_os = "linux")]
    {
        let ticks = process_start_ticks(pid)?;
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz <= 0 {
            return None;
        }
        let stat = fs::read_to_string("/proc/stat").ok()?;
        let btime: u64 = stat
            .lines()
            .find_map(|l| l.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        let hz = hz as u64;
        let since_boot = std::time::Duration::from_secs(ticks / hz)
            + std::time::Duration::from_nanos((ticks % hz) * 1_000_000_000 / hz);
        Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(btime) + since_boot)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Holders of exclusive `flock(2)` locks (`FLOCK ADVISORY WRITE`), keyed by
/// `(device major, device minor, inode)`, from `/proc/locks`. Shared, POSIX
/// and blocked-waiter (`->`) entries are ignored. Empty where the kernel does
/// not expose the table.
pub fn file_lock_holders() -> std::collections::HashMap<(u64, u64, u64), u32> {
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string("/proc/locks")
            .map(|s| parse_proc_locks(&s))
            .unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::collections::HashMap::new()
    }
}

fn parse_proc_locks(text: &str) -> std::collections::HashMap<(u64, u64, u64), u32> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        // "1: FLOCK  ADVISORY  WRITE 1234 fd:01:5678 0 EOF"
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 || cols[1..4] != ["FLOCK", "ADVISORY", "WRITE"] {
            continue;
        }
        let Ok(pid) = cols[4].parse::<u32>() else {
            continue;
        };
        let mut id = cols[5].splitn(3, ':');
        let (Some(maj), Some(min), Some(ino)) = (id.next(), id.next(), id.next()) else {
            continue;
        };
        if let (Ok(maj), Ok(min), Ok(ino)) = (
            u64::from_str_radix(maj, 16),
            u64::from_str_radix(min, 16),
            ino.parse::<u64>(),
        ) {
            map.insert((maj, min, ino), pid);
        }
    }
    map
}

/// `(device major, device minor, inode)` of `path`, matching the key used by
/// [`file_lock_holders`].
#[cfg(unix)]
pub fn file_lock_key(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).ok()?;
    let dev = meta.dev();
    // Linux `dev_t` encoding (glibc/musl `major()` / `minor()`).
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    Some((major, minor, meta.ino()))
}

#[cfg(not(unix))]
pub fn file_lock_key(_path: &Path) -> Option<(u64, u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_home_is_path_boundary_aware() {
        let home = Path::new("/home/al");
        assert_eq!(strip_home("/home/al/repo", home), "repo");
        assert_eq!(strip_home("/home/al", home), "");
        assert_eq!(strip_home("/home/alice/repo", home), "/home/alice/repo");
    }

    #[test]
    fn safe_cli_id_rejects_option_like_ids() {
        assert!(is_safe_cli_id("0192-aaaa"));
        assert!(!is_safe_cli_id("--always-approve"));
        assert!(!is_safe_cli_id(""));
    }

    #[test]
    fn tagged_user_body_extracts_or_passes_through() {
        assert_eq!(
            tagged_user_body(
                "<user_query>\nhi\n</user_query>\n<meta>x</meta>",
                "user_query"
            ),
            Some("hi")
        );
        assert_eq!(tagged_user_body("plain", "user_query"), Some("plain"));
        // a longer tag sharing the prefix must not match
        assert_eq!(
            tagged_user_body("<user_query_extra>x</user_query_extra>", "user_query"),
            None
        );
        assert_eq!(
            tagged_user_body(
                "<USER_REQUEST>\nこんにちは\n</USER_REQUEST>\n<ADDITIONAL_METADATA>t</ADDITIONAL_METADATA>",
                "USER_REQUEST"
            ),
            Some("こんにちは")
        );
        assert_eq!(
            tagged_user_body("<system>injected</system>", "user_query"),
            None
        );
    }

    #[test]
    fn stat_start_ticks_skips_parenthesized_comm() {
        let mut stat = String::from("1234 (we ird) name) S");
        for n in 4..=21 {
            stat.push_str(&format!(" {}", n));
        }
        stat.push_str(" 987654 23 24");
        assert_eq!(parse_stat_start_ticks(&stat), Some(987654));
        assert_eq!(parse_stat_start_ticks("1 (x) S 1 2"), None);
        assert_eq!(parse_stat_start_ticks("garbage"), None);
    }

    #[test]
    fn proc_locks_keeps_exclusive_flock_holders_only() {
        let text = "1: FLOCK  ADVISORY  WRITE 165470 103:02:23794220 0 EOF\n\
                    1: -> FLOCK  ADVISORY  WRITE 999 103:02:23794220 0 EOF\n\
                    2: POSIX  ADVISORY  READ 42 103:02:23794220 0 EOF\n\
                    3: FLOCK  ADVISORY  READ 43 00:2c:77 0 EOF\n\
                    4: POSIX  ADVISORY  WRITE 44 00:2c:78 0 EOF\n\
                    bad line\n";
        let map = parse_proc_locks(text);
        assert_eq!(map.get(&(0x103, 0x02, 23794220)), Some(&165470));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn stat_state_reads_field_after_comm() {
        assert_eq!(parse_stat_state("12 (co)dex) Z 1 2"), Some('Z'));
        assert_eq!(parse_stat_state("12 (codex) S 1 2"), Some('S'));
        assert_eq!(parse_stat_state("garbage"), None);
        // A comm cut mid-character is not UTF-8; lossy decoding keeps the
        // fields after it intact.
        let raw = b"12 (a\xe3\x81) Z 1 2";
        assert_eq!(parse_stat_state(&String::from_utf8_lossy(raw)), Some('Z'));
    }

    #[test]
    fn json_pid_accepts_numbers_and_numeric_strings() {
        let v = serde_json::json!({"a": 12, "b": "34", "c": 0, "d": "x", "e": -1});
        assert_eq!(json_pid(&v, "a"), Some(12));
        assert_eq!(json_pid(&v, "b"), Some(34));
        assert_eq!(json_pid(&v, "c"), None);
        assert_eq!(json_pid(&v, "d"), None);
        assert_eq!(json_pid(&v, "e"), None);
        assert_eq!(json_pid(&v, "missing"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_start_is_known_and_in_the_past() {
        let pid = std::process::id();
        assert!(is_pid_alive(pid));
        assert!(process_start_ticks(pid).is_some());
        let started = process_start_time(pid).unwrap();
        assert!(started <= SystemTime::now());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreaped_child_is_not_alive() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        // Leave the exited child unreaped so it stays a zombie.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !is_zombie(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(is_zombie(pid));
        assert!(!is_pid_alive(pid));
        child.wait().unwrap();
    }

    #[test]
    fn percent_decode_handles_invalid_escapes() {
        assert_eq!(percent_decode("%2Fdata%2Fhome"), "/data/home");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }
}

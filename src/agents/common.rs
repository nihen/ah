use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use memmap2::Mmap;
use regex::Regex;

/// Resolve and canonicalize the user's home directory.
pub fn canonical_home() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    fs::canonicalize(&home).unwrap_or(home)
}

pub fn format_mtime(mtime: SystemTime) -> String {
    crate::datetime::format_local_minutes(mtime)
}

/// Lowercase hex of a digest, as `{:x}` printed it before sha2 0.11. Hashes
/// end up in search positions and cache paths, so the format must not change.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Unix seconds of a timestamp as agents write it: an RFC 3339 string, or a
/// number of seconds or milliseconds since the epoch.
pub fn unix_time(val: &serde_json::Value) -> Option<i64> {
    // Seconds stay below this until the year 5138; milliseconds passed it
    // in 1973.
    const MILLIS_FROM: f64 = 1e11;
    match val {
        serde_json::Value::String(s) => {
            crate::datetime::parse_rfc3339(s).map(crate::datetime::unix_seconds)
        }
        serde_json::Value::Number(n) => {
            let n = n.as_f64().filter(|n| n.is_finite() && *n > 0.0)?;
            Some(if n >= MILLIS_FROM { n / 1000.0 } else { n } as i64)
        }
        _ => None,
    }
}

/// Strip the home directory prefix from `path` (path-boundary aware:
/// `/home/al` does not match `/home/alice/repo`).
pub fn strip_home(path: &str, home: &Path) -> String {
    let home_str = home.to_string_lossy();
    let home_str = home_str.trim_end_matches('/');
    if let Some(rest) = path.strip_prefix(home_str)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return rest.trim_start_matches('/').to_string();
    }
    path.to_string()
}

/// Canonicalize `path` if it exists on disk (resolves symlinks such as
/// `/home/user` → `/data/home/user` so that it matches the canonicalized cwd
/// filter); otherwise return it unchanged.
pub fn canonicalize_if_exists(path: &str) -> String {
    fs::canonicalize(path).map_or_else(|_| path.to_string(), |p| p.to_string_lossy().to_string())
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
    /// Bytes that hold a resource (e.g. a slot limiting the decoded logs
    /// held at once) until they are dropped.
    Guarded {
        bytes: Vec<u8>,
        _guard: Box<dyn std::any::Any>,
    },
}

impl std::ops::Deref for SessionBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            SessionBytes::Mmap(m) => m,
            SessionBytes::Owned(v) | SessionBytes::Guarded { bytes: v, .. } => v,
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

/// Distinct model names in first-use order plus the model that answered
/// last, for `AgentPlugin::resolve_models`. `auto` is a selection, not a
/// model, and is skipped.
#[derive(Debug, Default)]
pub struct ModelList {
    all: Vec<String>,
    last: Option<String>,
    /// Models metered so far (see `push_metered`).
    metered: Vec<String>,
    /// Replies in this run since its last metering: one naming its model,
    /// one not.
    named_since_metered: bool,
    unnamed_since_metered: bool,
}

fn is_model(model: &str) -> bool {
    !model.is_empty() && model != "auto"
}

impl ModelList {
    /// Record a reply by `model`; replies must be pushed in order.
    pub fn push(&mut self, model: &str) {
        let model = model.trim();
        if !is_model(model) {
            return;
        }
        if !self.all.iter().any(|m| m == model) {
            self.all.push(model.to_string());
        }
        self.last = Some(model.to_string());
        self.named_since_metered = true;
    }

    /// Record a reply that names no model.
    pub fn push_unnamed(&mut self) {
        self.unnamed_since_metered = true;
    }

    /// Record the start of a run (a new or resumed session); replies of a
    /// run that ended without metering do not carry over.
    pub fn start_run(&mut self) {
        self.named_since_metered = false;
        self.unnamed_since_metered = false;
    }

    /// Record the models that received requests in this run since its last
    /// metering, in first-use order. When only replies naming no model came
    /// in between, the last of them stands in for the model that answered;
    /// a named reply is never overridden. `merge_metered` folds them in.
    pub fn push_metered(&mut self, models: &[String]) {
        let start = self.metered.len();
        self.metered.extend(
            models
                .iter()
                .map(|m| m.trim().to_string())
                .filter(|m| is_model(m)),
        );
        if self.unnamed_since_metered
            && !self.named_since_metered
            && let Some(model) = self.metered[start..].last()
        {
            self.last = Some(model.clone());
        }
        self.named_since_metered = false;
        self.unnamed_since_metered = false;
    }

    /// Fold in the metered models replies do not name: each goes before the
    /// first known model that follows it in metering order, or last. Without
    /// any reply to go by, the last metered model stands in for the last one
    /// that answered.
    pub fn merge_metered(&mut self) {
        let mut order = ModelList::default();
        self.metered.iter().for_each(|model| order.push(model));
        for (i, model) in order.all.iter().enumerate() {
            if self.all.contains(model) {
                continue;
            }
            let at = order.all[i + 1..]
                .iter()
                .find_map(|next| self.all.iter().position(|m| m == next))
                .unwrap_or(self.all.len());
            self.all.insert(at, model.clone());
        }
        if self.last.is_none() {
            self.last = order.last;
        }
    }

    /// `None` when no model was recorded.
    pub fn non_empty(self) -> Option<Self> {
        (!self.all.is_empty()).then_some(self)
    }

    /// The model that answered last.
    pub fn last(&self) -> &str {
        self.last.as_deref().unwrap_or_default()
    }

    /// All models in first-use order.
    pub fn all(&self) -> &[String] {
        &self.all
    }

    /// All models, comma-separated in first-use order.
    pub fn joined(&self) -> String {
        self.all.join(",")
    }
}

/// Parse a JSON object keeping only the values at `paths` (each a list of
/// object keys from the root); every other value is validated like
/// `serde_json::Value` would, but not built, so the same lines parse. A
/// value that is not an object where a path continues reads as `null`:
/// `Value::pointer` along `paths` sees what it would in the full value.
pub fn parse_json_paths(line: &[u8], paths: &[&[&str]]) -> Option<serde_json::Value> {
    use serde::de::DeserializeSeed;
    let mut de = serde_json::Deserializer::from_slice(line);
    let val = JsonPaths(paths.to_vec()).deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(val)
}

/// Remainders of the paths that pass through the value being parsed.
struct JsonPaths<'p>(Vec<&'p [&'p str]>);

impl<'de> serde::de::DeserializeSeed<'de> for JsonPaths<'_> {
    type Value = serde_json::Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        if self.0.iter().any(|path| path.is_empty()) {
            return serde::Deserialize::deserialize(de);
        }
        de.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for JsonPaths<'_> {
    type Value = serde_json::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut kept = serde_json::Map::new();
        while let Some(rest) = map.next_key_seed(PathsUnderKey(&self.0))? {
            match rest.first() {
                Some((key, _)) => {
                    let rest = rest.iter().map(|(_, rest)| *rest).collect();
                    kept.insert(key.to_string(), map.next_value_seed(JsonPaths(rest))?);
                }
                None => {
                    map.next_value::<Skipped>()?;
                }
            }
        }
        Ok(serde_json::Value::Object(kept))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<Skipped>()?.is_some() {}
        Ok(serde_json::Value::Null)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
}

/// Reads an object key as the paths that start with it: the key and the
/// rest of each. Empty for a key no path goes through.
struct PathsUnderKey<'a, 'p>(&'a [&'p [&'p str]]);

impl<'de, 'p> serde::de::DeserializeSeed<'de> for PathsUnderKey<'_, 'p> {
    type Value = Vec<(&'p str, &'p [&'p str])>;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_str(self)
    }
}

impl<'p> serde::de::Visitor<'_> for PathsUnderKey<'_, 'p> {
    type Value = Vec<(&'p str, &'p [&'p str])>;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an object key")
    }
    fn visit_str<E>(self, key: &str) -> Result<Self::Value, E> {
        Ok(self
            .0
            .iter()
            .filter(|path| path[0] == key)
            .map(|path| (path[0], &path[1..]))
            .collect())
    }
}

/// A JSON value that is read and dropped. Unlike `serde::de::IgnoredAny`,
/// which `serde_json` skips without decoding, strings and numbers are
/// decoded and the nesting limit applies, as for `serde_json::Value`.
struct Skipped;

impl<'de> serde::Deserialize<'de> for Skipped {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(Skipped)
    }
}

impl<'de> serde::de::Visitor<'de> for Skipped {
    type Value = Skipped;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<Skipped, Skipped>()?.is_some() {}
        Ok(Skipped)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<Skipped>()?.is_some() {}
        Ok(Skipped)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Skipped)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Skipped)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Skipped)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Skipped)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Skipped)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Skipped)
    }
}

/// The lines of `data`, as `data.split(|&b| b == b'\n')` yields them, found
/// with SIMD search rather than a byte at a time.
pub fn jsonl_lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = Some(data);
    std::iter::from_fn(move || {
        let data = rest?;
        let (line, after) = match memchr::memchr(b'\n', data) {
            Some(end) => (&data[..end], Some(&data[end + 1..])),
            None => (data, None),
        };
        rest = after;
        Some(line)
    })
}

/// Finds the JSONL lines of `data` that contain a needle, searching the
/// buffer for the needle rather than splitting it into lines first: the
/// bytes of lines without it are read once, not once per search.
pub struct LinesWith<'a, 'n> {
    data: &'a [u8],
    finder: memchr::memmem::Finder<'n>,
    /// The last line found (`None` once the needle does not occur again),
    /// reused while the caller has not passed it.
    found: Option<Option<std::ops::Range<usize>>>,
}

impl<'a, 'n> LinesWith<'a, 'n> {
    pub fn new(data: &'a [u8], needle: &'n [u8]) -> Self {
        Self {
            data,
            finder: memchr::memmem::Finder::new(needle),
            found: None,
        }
    }

    /// Byte range (without the trailing `\n`) of the first line starting at
    /// or after `from` that contains the needle. `from` must be the start of
    /// a line and must not decrease between calls.
    pub fn next_from(&mut self, from: usize) -> Option<std::ops::Range<usize>> {
        match &self.found {
            Some(None) => return None,
            Some(Some(line)) if line.start >= from => return Some(line.clone()),
            _ => {}
        }
        let data = self.data;
        let line = data
            .get(from..)
            .and_then(|rest| self.finder.find(rest))
            .map(|pos| {
                let at = from + pos;
                let start = memchr::memrchr(b'\n', &data[from..at]).map_or(from, |i| from + i + 1);
                let end = memchr::memchr(b'\n', &data[at..]).map_or(data.len(), |i| at + i);
                start..end
            });
        self.found = Some(line.clone());
        line
    }
}

/// Models from the JSONL lines that contain `needle`; other lines are
/// skipped without parsing. `model_of` sees only the values at `paths`
/// (see `parse_json_paths`).
pub fn jsonl_models(
    path: &Path,
    needle: &[u8],
    paths: &[&[&str]],
    model_of: impl for<'a> Fn(&'a serde_json::Value) -> Option<&'a str>,
) -> Option<ModelList> {
    let data = mmap_file(path)?;
    let mut lines = LinesWith::new(&data, needle);
    let mut models = ModelList::default();
    let mut from = 0;
    while let Some(line) = lines.next_from(from) {
        from = line.end + 1;
        if let Some(val) = parse_json_paths(&data[line], paths)
            && let Some(model) = model_of(&val)
        {
            models.push(model);
        }
    }
    models.non_empty()
}

pub fn for_each_jsonl_value_bytes(data: &[u8], mut visit: impl FnMut(&serde_json::Value) -> bool) {
    for line_bytes in jsonl_lines(data) {
        if line_bytes.len() < 2 {
            continue;
        }
        if let Ok(line) = std::str::from_utf8(line_bytes)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && !visit(&val)
        {
            return;
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
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
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
#[cfg(any(target_os = "linux", test))]
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
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        let read = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).ok()?,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let micros = u32::try_from(info.pbi_start_tvusec).ok()?;
        Some(
            SystemTime::UNIX_EPOCH
                + std::time::Duration::new(info.pbi_start_tvsec, micros.checked_mul(1000)?),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
    proc_locks()
        .map(|table| parse_proc_locks(&table))
        .unwrap_or_default()
}

/// Processes with a connection open to a SQLite database in WAL mode, keyed
/// by the `(device major, device minor, inode)` of its `-shm` file. SQLite
/// holds a shared POSIX lock on byte 128 of that file for as long as a
/// connection is open, so any reader of the database is listed, not only
/// its owner. Empty where the kernel does not expose the lock table.
pub fn sqlite_wal_holders() -> std::collections::HashMap<(u64, u64, u64), Vec<u32>> {
    proc_locks()
        .map(|table| parse_sqlite_wal_holders(&table))
        .unwrap_or_default()
}

fn proc_locks() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string("/proc/locks").ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// One held lock of `/proc/locks`: `(kind, access, pid, file, start, end)`,
/// e.g. `1: FLOCK  ADVISORY  WRITE 1234 fd:01:5678 0 EOF`. Blocked waiters
/// (`1: -> FLOCK ...`) are skipped.
fn proc_lock_rows(
    text: &str,
) -> impl Iterator<Item = (&str, &str, u32, (u64, u64, u64), &str, &str)> {
    text.lines().filter_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 8 || cols[2] != "ADVISORY" {
            return None;
        }
        let pid = cols[4].parse::<u32>().ok()?;
        let mut id = cols[5].splitn(3, ':');
        let file = (
            u64::from_str_radix(id.next()?, 16).ok()?,
            u64::from_str_radix(id.next()?, 16).ok()?,
            id.next()?.parse::<u64>().ok()?,
        );
        Some((cols[1], cols[3], pid, file, cols[6], cols[7]))
    })
}

fn parse_proc_locks(text: &str) -> std::collections::HashMap<(u64, u64, u64), u32> {
    proc_lock_rows(text)
        .filter(|row| (row.0, row.1) == ("FLOCK", "WRITE"))
        .map(|row| (row.3, row.2))
        .collect()
}

fn parse_sqlite_wal_holders(text: &str) -> std::collections::HashMap<(u64, u64, u64), Vec<u32>> {
    let mut map: std::collections::HashMap<_, Vec<u32>> = std::collections::HashMap::new();
    for row in proc_lock_rows(text) {
        // The kernel merges this lock with the adjacent read marks the same
        // process holds (bytes 123 to 127), so the range only has to cover
        // byte 128.
        let covers = row.4.parse::<u64>().is_ok_and(|start| start <= 128)
            && (row.5 == "EOF" || row.5.parse::<u64>().is_ok_and(|end| end >= 128));
        if (row.0, row.1) == ("POSIX", "READ") && covers {
            let pids = map.entry(row.3).or_default();
            if !pids.contains(&row.2) {
                pids.push(row.2);
            }
        }
    }
    map
}

/// The arguments of a running process, the program first. `None` where
/// they cannot be read.
pub fn process_args(pid: u32) -> Option<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(
            raw.split(|b| *b == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// The name of a running process: the basename of its executable, cut to 15
/// bytes on Linux. `None` where it cannot be read.
pub fn process_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(comm.trim_end_matches('\n').to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let mut name = [0u8; 256];
        let len = unsafe {
            libc::proc_name(
                i32::try_from(pid).ok()?,
                name.as_mut_ptr().cast(),
                name.len() as u32,
            )
        };
        let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
        Some(String::from_utf8_lossy(name.get(..len)?).into_owned())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Whether a running process has `file` open. `None` where its open files
/// cannot be listed.
pub fn process_has_open(pid: u32, file: &Path) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let wanted = fs::metadata(file).ok()?;
        let open = fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
        Some(open.flatten().any(|fd| {
            fs::metadata(fd.path())
                .is_ok_and(|meta| (meta.dev(), meta.ino()) == (wanted.dev(), wanted.ino()))
        }))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, file);
        None
    }
}

/// Every process id. Empty where the process table cannot be listed.
pub fn all_pids() -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
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

    fn models(list: &[&str]) -> ModelList {
        let mut models = ModelList::default();
        list.iter().for_each(|m| models.push(m));
        models
    }

    #[test]
    fn merge_metered_places_new_models_by_their_successors() {
        let order = |list: &[&str]| list.iter().map(|m| m.to_string()).collect::<Vec<_>>();
        let merged = |seen: &[&str], ordered: &[&str]| {
            let mut list = models(seen);
            list.push_metered(&order(ordered));
            list.merge_metered();
            list.non_empty().map(|m| (m.joined(), m.last().to_string()))
        };
        let pair = |all: &str, last: &str| Some((all.to_string(), last.to_string()));
        assert_eq!(merged(&["B"], &["A", "B"]), pair("A,B", "B"));
        assert_eq!(merged(&["A", "B"], &["B"]), pair("A,B", "B"));
        assert_eq!(merged(&[], &["A", "B", "A", "B"]), pair("A,B", "B"));
        assert_eq!(merged(&[], &["A", "B", "A"]), pair("A,B", "A"));
        assert_eq!(merged(&["A"], &["A", "B"]), pair("A,B", "A"));
        assert_eq!(merged(&["C"], &["A", "auto", "", "B"]), pair("C,A,B", "C"));
        assert_eq!(merged(&[], &[]), None);
        // Metering stands in only for spans with unnamed replies alone.
        let mut list = models(&["A"]);
        list.push_unnamed();
        list.push_metered(&order(&["A", "B"]));
        assert_eq!(list.last(), "A");
        list.push_unnamed();
        list.push_metered(&order(&["C"]));
        list.push_unnamed();
        list.push_metered(&order(&["B"]));
        list.merge_metered();
        assert_eq!((list.joined().as_str(), list.last()), ("A,B,C", "B"));
    }

    #[test]
    fn model_list_last_is_the_latest_reply() {
        let list = models(&["A", "B", "A", "auto", " "]);
        assert_eq!((list.joined().as_str(), list.last()), ("A,B", "A"));
    }
    use super::*;

    #[test]
    fn hex_matches_lower_hex_digest_format() {
        use sha2::{Digest, Sha256};
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn parse_json_paths_keeps_only_the_paths() {
        let line = br#"{"a":{"b":"x","c":[1,{"d":2}]},"t":"k","big":"skipped","n":{"m":1}}"#;
        let val = parse_json_paths(line, &[&["t"], &["a", "b"], &["n"]]).unwrap();
        assert_eq!(val, serde_json::json!({"a":{"b":"x"},"t":"k","n":{"m":1}}));
    }

    #[test]
    fn parse_json_paths_reads_like_the_full_value() {
        let paths: &[&[&str]] = &[&["a", "b"]];
        // Not an object where the path continues: present, without `b`.
        for line in [
            r#"{"a":"s"}"#,
            r#"{"a":[{"b":1}]}"#,
            r#"{"a":null}"#,
            r#"{"a":1.5}"#,
        ] {
            let val = parse_json_paths(line.as_bytes(), paths).unwrap();
            assert!(val.get("a").is_some(), "{line}");
            assert!(val.pointer("/a/b").is_none(), "{line}");
        }
        // The last of duplicate keys wins.
        let val = parse_json_paths(br#"{"a":{"b":1},"a":{"b":2}}"#, paths).unwrap();
        assert_eq!(val.pointer("/a/b"), Some(&serde_json::json!(2)));
    }

    /// Whatever sits outside the paths, a line parses exactly when it
    /// parses as a full `Value`.
    #[test]
    fn parse_json_paths_accepts_the_lines_value_accepts() {
        let paths: &[&[&str]] = &[&["a", "b"]];
        let nested = |depth: usize| {
            format!(
                r#"{{"a":{{"b":1}},"x":{}0{}}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            )
        };
        let nested_objects = |depth: usize| {
            format!(
                r#"{{"a":{{"b":1}},"x":{}0{}}}"#,
                r#"{"k":"#.repeat(depth),
                "}".repeat(depth)
            )
        };
        let lines: Vec<Vec<u8>> = [
            r#"{"a":{"b":1}}"#,
            r#"{"a":{"b":1}"#,
            r#"{"a":{"b":1}} x"#,
            r#"{"a":{"b":1}}  "#,
            r#"{"x":[1,}"#,
            "",
            "[]",
            r#"{"a":{"b":1},"x":"\ud800"}"#,
            r#"{"a":{"b":1},"x":"\ud83d\ude00"}"#,
            r#"{"a":{"b":1},"\ud800":1}"#,
            r#"{"a":{"b":1},"x":1e999}"#,
            r#"{"a":{"b":1},"x":[1e999]}"#,
            r#"{"a":{"b":1},"x":123456789012345678901234567890}"#,
            r#"{"a":{"b":1},"x":01}"#,
            r#"{"a":{"b":1},"x":"\q"}"#,
            "{\"a\":{\"b\":1},\"x\":\"\t\"}",
            r#"{"a":{"b":1},"x":{"y":tru}}"#,
            r#"{"a":{"b":1},"x":{1:2}}"#,
            r#"{"a":[[1e999]]}"#,
        ]
        .iter()
        .map(|line| line.as_bytes().to_vec())
        .chain([b"{\"a\":{\"b\":1},\"x\":\"\xff\"}".to_vec()])
        .chain([b"{\"a\":{\"b\":1},\"\xff\":1}".to_vec()])
        .chain((120..135).map(|depth| nested(depth).into_bytes()))
        .chain((120..135).map(|depth| nested_objects(depth).into_bytes()))
        .collect();
        let mut accepted = 0;
        for line in &lines {
            let full = serde_json::from_slice::<serde_json::Value>(line).ok();
            let kept = parse_json_paths(line, paths);
            let shown = String::from_utf8_lossy(line);
            assert_eq!(kept.is_some(), full.is_some(), "{shown}");
            if let (Some(kept), Some(full)) = (kept, full) {
                assert_eq!(kept.pointer("/a/b"), full.pointer("/a/b"), "{shown}");
                accepted += 1;
            }
        }
        // Both outcomes are covered.
        assert!(accepted > 10 && accepted < lines.len() - 10);
    }

    #[test]
    fn jsonl_lines_matches_split() {
        for data in ["", "\n", "a", "a\n", "a\n\nb", "\na\nb\n"] {
            let data = data.as_bytes();
            let expected: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
            assert_eq!(jsonl_lines(data).collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn lines_with_finds_the_lines_a_line_scan_finds() {
        for data in [
            "",
            "x",
            "ab\n",
            "ab",
            "a\nab\nb\nxabx ab\n\nab",
            "\nab\nab\n",
            "abab\nb\n",
        ] {
            let data = data.as_bytes();
            let expected: Vec<&[u8]> = jsonl_lines(data)
                .filter(|line| memchr::memmem::find(line, b"ab").is_some())
                .collect();
            let mut lines = LinesWith::new(data, b"ab");
            let mut found = Vec::new();
            let mut from = 0;
            while let Some(line) = lines.next_from(from) {
                // asking again before moving on gives the same line
                assert_eq!(lines.next_from(from), Some(line.clone()));
                from = line.end + 1;
                found.push(&data[line]);
            }
            assert_eq!(found, expected);
        }
    }

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

    #[cfg(target_os = "linux")]
    #[test]
    fn open_files_of_this_process_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (held, other) = (tmp.path().join("held"), tmp.path().join("other"));
        std::fs::write(&held, "").unwrap();
        std::fs::write(&other, "").unwrap();
        let _file = std::fs::File::open(&held).unwrap();
        let pid = std::process::id();
        assert_eq!(process_has_open(pid, &held), Some(true));
        assert_eq!(process_has_open(pid, &other), Some(false));
        assert_eq!(process_has_open(pid, &tmp.path().join("missing")), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_has_a_name_and_a_start_time() {
        let pid = std::process::id();
        let started = process_start_time(pid).unwrap();
        let age = std::time::SystemTime::now()
            .duration_since(started)
            .unwrap();
        assert!(age < std::time::Duration::from_secs(3600), "{age:?}");
        assert!(!process_name(pid).unwrap().is_empty());
    }

    #[test]
    fn sqlite_wal_holders_are_the_readers_of_the_shm_file() {
        let text = "1: POSIX  ADVISORY  READ 10 fc:00:500 128 128\n\
                    2: POSIX  ADVISORY  READ 11 fc:00:500 128 128\n\
                    3: POSIX  ADVISORY  READ 10 fc:00:499 1073741826 1073742335\n\
                    4: POSIX  ADVISORY  WRITE 12 fc:00:501 128 128\n\
                    5: FLOCK  ADVISORY  READ 13 fc:00:502 128 128\n\
                    6: -> POSIX  ADVISORY  READ 14 fc:00:500 128 128\n";
        let map = parse_sqlite_wal_holders(text);
        assert_eq!(map.get(&(0xfc, 0, 500)), Some(&vec![10, 11]));
        assert_eq!(map.len(), 1);

        // Merged with the read mark next to it, and listed once per process.
        let merged = "1: POSIX  ADVISORY  READ 20 fc:00:600 127 128\n\
                      2: POSIX  ADVISORY  READ 20 fc:00:600 124 124\n\
                      3: POSIX  ADVISORY  READ 21 fc:00:600 0 EOF\n\
                      4: POSIX  ADVISORY  READ 22 fc:00:600 129 EOF\n\
                      5: POSIX  ADVISORY  READ 23 fc:00:600 120 127\n\
                      6: POSIX  ADVISORY  READ 0 fc:00:600 128 128\n\
                      7: POSIX  ADVISORY  READ 0 fc:00:600 128 128\n";
        let map = parse_sqlite_wal_holders(merged);
        assert_eq!(map.get(&(0xfc, 0, 600)), Some(&vec![20, 21, 0]));
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

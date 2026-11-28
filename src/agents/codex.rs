use super::entries::{RecordVisitor, SearchKind as K, TypedVisitor};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::SessionBytes;
use super::common::format_mtime;
use super::common::strip_home;
use super::common::{visit_all_strings, visit_tool_call, visit_tool_output};
use super::zstd_log::{self, Log};
use super::{MemoryKind, MemorySource};

static RE_CODEX_SESSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".*/sessions/(\d+/\d+/\d+)/.*").unwrap());
static RE_CODEX_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"rollout-(\d{4}-\d{2}-\d{2})T(\d{2})-(\d{2})").unwrap());
static RE_CODEX_ROLLOUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^rollout-[\dT-]+-(.+)$").unwrap());

/// Whether a rollout is compressed (`rollout-*.jsonl.zst`). Codex writes a
/// rollout as JSONL and may later replace a finished one with its
/// Zstandard-compressed copy (the `local_thread_store_compression` feature).
fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "zst")
}

/// Largest compressed rollout that is decoded whole, as text (`session_bytes`
/// and `raw_content`, e.g. for a raw search). Long sessions reach several
/// hundred megabytes (one of 940 MiB has been seen); the limit keeps a
/// damaged or hostile file from exhausting memory.
const MAX_ROLLOUT_BYTES: u64 = 2 << 30;
/// How much text of a compressed rollout is read line by line. Only one
/// line is held at a time, so this bounds the time a damaged or hostile
/// file can take, not memory.
const MAX_STREAMED_BYTES: u64 = 64 << 30;
/// Longest line read from a compressed rollout; a longer one is skipped.
/// Tool outputs make lines of ten megabytes or so.
const MAX_LINE_BYTES: usize = 256 << 20;
/// Largest `session_meta` line decoded from a compressed rollout. The line
/// carries the base instructions, a few kilobytes.
const MAX_META_LINE_BYTES: u64 = 16 << 20;

/// The rollout as JSONL text: the file itself, or its decoded content.
fn read_rollout(path: &Path) -> Option<Log> {
    zstd_log::read_log(path, is_compressed(path), MAX_ROLLOUT_BYTES)
}

/// Visit the JSONL lines of a rollout until `visit` returns `false`. A
/// compressed rollout is decoded one line at a time.
fn for_each_line(path: &Path, mut visit: impl FnMut(&[u8]) -> bool) {
    let Some(data) = super::common::mmap_file(path) else {
        return;
    };
    if is_compressed(path) {
        zstd_log::for_each_line(&data, MAX_STREAMED_BYTES, MAX_LINE_BYTES, visit);
    } else {
        for line in super::common::jsonl_lines(&data) {
            if !visit(line) {
                return;
            }
        }
    }
}

/// The nearest `sessions` or `archived_sessions` directory containing a
/// session file.
fn sessions_root(path: &Path) -> Option<&Path> {
    path.ancestors().find(|dir| {
        matches!(
            dir.file_name().and_then(|s| s.to_str()),
            Some("sessions" | "archived_sessions")
        )
    })
}

/// Codex home of a session file: the parent of its sessions root.
fn colocated_codex_home(path: &Path) -> Option<&Path> {
    sessions_root(path).and_then(|dir| dir.parent())
}

type TitleIndex = HashMap<String, String>;

/// Size and mtime of an index file; `None` when it does not exist.
type IndexStamp = Option<(u64, Option<SystemTime>)>;

/// Parsed `session_index.jsonl` files, keyed by path. Every listed session
/// needs a title, so an index is re-read only when its size or mtime
/// changes (e.g. a rename while `ah log -i` waits for a selection).
type TitleIndexCache = HashMap<PathBuf, (IndexStamp, Arc<TitleIndex>)>;

static TITLE_INDEXES: LazyLock<Mutex<TitleIndexCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn title_index(index_path: &Path) -> Arc<TitleIndex> {
    let stamp = fs::metadata(index_path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    let mut cache = TITLE_INDEXES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached_stamp, index)) = cache.get(index_path)
        && *cached_stamp == stamp
    {
        return index.clone();
    }
    let index = Arc::new(read_title_index(index_path));
    cache.insert(index_path.to_path_buf(), (stamp, index.clone()));
    index
}

/// Map each session id to its latest `thread_name`. Codex appends a new
/// line on every rename, so later lines win.
fn read_title_index(index_path: &Path) -> TitleIndex {
    let mut names = TitleIndex::new();
    let Ok(index_file) = fs::File::open(index_path) else {
        return names;
    };
    for line in BufReader::new(index_file).lines() {
        let Ok(line) = line else { break };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = val.get("id").and_then(|v| v.as_str());
        let name = val.get("thread_name").and_then(|v| v.as_str());
        if let (Some(id), Some(name)) = (id, name)
            && !name.is_empty()
        {
            names.insert(id.to_string(), name.to_string());
        }
    }
    names
}

/// Latest thread name of `session_id`: from the index next to the session
/// file, then from the configured Codex base (`CODEX_HOME` or `~/.codex`).
fn latest_thread_name(path: &Path, session_id: &str) -> Option<String> {
    static CONFIGURED_BASE: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| crate::config::resolve_agent_base("codex"));
    latest_thread_name_in(path, session_id, CONFIGURED_BASE.as_deref())
}

fn latest_thread_name_in(
    path: &Path,
    session_id: &str,
    configured: Option<&Path>,
) -> Option<String> {
    let mut homes = colocated_codex_home(path)
        .into_iter()
        .chain(configured)
        .collect::<Vec<_>>();
    homes.dedup();
    homes.iter().find_map(|home| {
        title_index(&home.join("session_index.jsonl"))
            .get(session_id)
            .cloned()
    })
}

/// Visit the user and assistant messages of a rollout. Only `response_item`
/// lines that name a message role can hold one, so other lines are skipped
/// before JSON parsing. This matters when the title falls back to the first
/// prompt: sessions without a real user prompt are scanned to the end.
fn for_each_message(data: &[u8], visit: &mut dyn FnMut(Message) -> bool) {
    for line in super::common::jsonl_lines(data) {
        if !visit_message_line(line, visit) {
            return;
        }
    }
}

/// Messages of one rollout line; `false` stops the iteration.
fn visit_message_line(line: &[u8], visit: &mut dyn FnMut(Message) -> bool) -> bool {
    if RESPONSE_ITEM.find(line).is_none()
        || (USER.find(line).is_none() && ASSISTANT.find(line).is_none())
    {
        return true;
    }
    match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(val) => visit_message_value(&val, visit),
        Err(_) => true,
    }
}

static RESPONSE_ITEM: LazyLock<memchr::memmem::Finder<'static>> =
    LazyLock::new(|| memchr::memmem::Finder::new(b"\"response_item\""));
static USER: LazyLock<memchr::memmem::Finder<'static>> =
    LazyLock::new(|| memchr::memmem::Finder::new(b"\"user\""));
static ASSISTANT: LazyLock<memchr::memmem::Finder<'static>> =
    LazyLock::new(|| memchr::memmem::Finder::new(b"\"assistant\""));

/// Visit the search texts of a rollout: messages, tool calls and tool
/// outputs, all of which live in `response_item` lines.
fn for_each_search_text(data: &[u8], visit: &mut dyn FnMut(&str) -> bool) {
    for line in super::common::jsonl_lines(data) {
        if !visit_search_line(line, visit) {
            return;
        }
    }
}

/// Search texts of one rollout line; `false` stops the iteration.
fn visit_search_line(line: &[u8], visit: &mut dyn FnMut(&str) -> bool) -> bool {
    if !may_hold_search_text(line) {
        return true;
    }
    match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(val) => visit_search_value(&val, &mut |_, text| visit(text)),
        Err(_) => true,
    }
}

/// Cheap check that rules out lines without search texts before JSON
/// parsing: lines other than `response_item`, developer instructions,
/// encrypted reasoning, and user messages whose every text is injected
/// context (starting with `<` or `# `, which `visit_message_value` skips).
/// Keys are compared in their serialized form; inside a JSON string these
/// quotes would be escaped, so a match is always a real key. The line and
/// payload types are looked up in the line's head only (rollouts write
/// `timestamp`, `type`, then `payload` with its `type` and `role` first);
/// when the head does not identify the line, it is kept.
fn may_hold_search_text(line: &[u8]) -> bool {
    // Normalized Cloud records need not have the key order of Codex rollouts.
    if memchr::memmem::find(&line[..line.len().min(128)], b"\"ah_cloud_text\"").is_some() {
        return true;
    }
    use regex::bytes::Regex as BytesRegex;
    // JSON allows whitespace around `:`; `\s` also covers it inside the
    // `{` of `payload`.
    static TYPE_VALUE: LazyLock<BytesRegex> =
        LazyLock::new(|| BytesRegex::new(r#""type"\s*:\s*"([^"]*)""#).unwrap());
    // Roles are matched only in the payload's own message header (`type`,
    // then scalar fields such as `id`, then `role`), not in nested data.
    static SKIPPED_PAYLOAD: LazyLock<BytesRegex> = LazyLock::new(|| {
        BytesRegex::new(concat!(
            r#""payload"\s*:\s*\{\s*"type"\s*:\s*"message"\s*,[^{}\[\]]*"role"\s*:\s*"developer""#,
            r#"|"payload"\s*:\s*\{\s*"type"\s*:\s*"reasoning""#,
        ))
        .unwrap()
    });
    static USER_ROLE: LazyLock<BytesRegex> = LazyLock::new(|| {
        BytesRegex::new(
            r#""payload"\s*:\s*\{\s*"type"\s*:\s*"message"\s*,[^{}\[\]]*"role"\s*:\s*"user""#,
        )
        .unwrap()
    });
    static TEXT_VALUE: LazyLock<BytesRegex> =
        LazyLock::new(|| BytesRegex::new(r#""text"\s*:\s*""#).unwrap());
    const HEAD: usize = 512;
    let head = &line[..line.len().min(HEAD)];
    let Some(kind) = TYPE_VALUE.captures(head).and_then(|caps| caps.get(1)) else {
        return true;
    };
    // a type written with escapes cannot be read here: keep the line
    if kind.as_bytes().contains(&b'\\') {
        return true;
    }
    if kind.as_bytes() != b"response_item" || SKIPPED_PAYLOAD.is_match(head) {
        return false;
    }
    if !USER_ROLE.is_match(head) {
        return true;
    }
    TEXT_VALUE.find_iter(line).any(|m| {
        let text = &line[m.end()..];
        !(text.starts_with(b"<") || text.starts_with(b"# "))
    })
}

/// Search texts of one rollout line: its messages, the name and input of a
/// `*_call` item, or the output of a `*_call_output` item.
fn visit_search_value(val: &serde_json::Value, visit: &mut TypedVisitor<'_>) -> bool {
    if !visit_message_value(val, &mut |message| {
        visit(K::from_role(message.role), &message.text)
    }) {
        return false;
    }
    if val.get("type").and_then(|v| v.as_str()) != Some("response_item") {
        return true;
    }
    let Some(payload) = val.get("payload") else {
        return true;
    };
    let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if kind.ends_with("_call") {
        visit_tool_call(payload.get("name"), None, &mut |text| {
            visit(K::ToolInput, text)
        }) && ["arguments", "input", "action"].iter().all(|key| {
            payload
                .get(key)
                .is_none_or(|v| visit_all_strings(v, &mut |text| visit(K::ToolInput, text)))
        })
    } else if kind.ends_with("_call_output") {
        payload
            .get("output")
            .is_none_or(|v| visit_tool_output(v, &mut |text| visit(K::ToolOutput, text)))
    } else {
        true
    }
}

/// Visit the messages of one rollout line; `false` stops the iteration.
fn visit_message_value(val: &serde_json::Value, visit: &mut dyn FnMut(Message) -> bool) -> bool {
    if val.get("type").and_then(|v| v.as_str()) != Some("response_item") {
        return true;
    }
    let Some(contents) = val.pointer("/payload/content").and_then(|v| v.as_array()) else {
        return true;
    };
    match val.pointer("/payload/role").and_then(|v| v.as_str()) {
        Some("user") => {
            for item in contents {
                let is_user_text = matches!(
                    item.get("type").and_then(|v| v.as_str()),
                    Some("input_text" | "text")
                );
                if is_user_text
                    && let Some(text) = item.get("text").and_then(|v| v.as_str())
                    && (val
                        .pointer("/payload/ah_cloud_text")
                        .and_then(|v| v.as_bool())
                        == Some(true)
                        || (!text.starts_with('<') && !text.starts_with("# ")))
                    && !visit(Message::user(text.to_string()).at(val.get("timestamp")))
                {
                    return false;
                }
            }
        }
        Some("assistant") => {
            for item in contents {
                if item.get("type").and_then(|v| v.as_str()) == Some("output_text")
                    && let Some(text) = item.get("text").and_then(|v| v.as_str())
                    && !visit(Message::assistant(text.to_string()).at(val.get("timestamp")))
                {
                    return false;
                }
            }
        }
        _ => {}
    }
    true
}

/// The `session_meta` fields ah reads from the first line of a rollout.
/// Each string field is `None` unless it is a JSON string.
#[derive(Clone)]
struct SessionMeta {
    id: Option<String>,
    cwd: Option<String>,
    /// `source` names a subagent (a thread another thread spawned, or a
    /// review run on its behalf).
    subagent: bool,
    /// The spawning thread, when the source records it.
    parent_id: Option<String>,
    cloud_title: Option<String>,
    cloud_created_at: Option<f64>,
    /// When the session started (`payload.timestamp`).
    started_at: Option<SystemTime>,
}

/// Size and mtime of the cached file, and its metadata (`None` when the
/// first line is not JSON).
type CachedMeta = (IndexStamp, Option<SessionMeta>);

static SESSION_METAS: LazyLock<Mutex<HashMap<PathBuf, CachedMeta>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Session metadata from the first line, or `None` when that line cannot be
/// read as JSON. The collector checks for subagents and listing resolves
/// the id, cwd and title separately, possibly on other threads, so each
/// file's line is parsed once and re-read when its size or mtime changes.
fn session_meta(path: &Path) -> Option<SessionMeta> {
    let stamp = fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    if let Some((cached_stamp, meta)) = SESSION_METAS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        && *cached_stamp == stamp
    {
        return meta.clone();
    }
    let meta = read_session_meta(path);
    SESSION_METAS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), (stamp, meta.clone()));
    meta
}

fn read_session_meta(path: &Path) -> Option<SessionMeta> {
    let mut line = Vec::new();
    if is_compressed(path) {
        line = zstd_log::first_line(&super::common::mmap_file(path)?, MAX_META_LINE_BYTES)?;
    } else {
        let file = fs::File::open(path).ok()?;
        BufReader::new(file).read_until(b'\n', &mut line).ok()?;
    }
    let val = serde_json::from_slice::<serde_json::Value>(&line).ok()?;
    let field = |pointer| {
        val.pointer(pointer)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let subagent = val.pointer("/payload/source/subagent").is_some();
    Some(SessionMeta {
        id: field("/payload/id"),
        cloud_title: field("/payload/cloud_title"),
        cloud_created_at: val
            .pointer("/payload/cloud_created_at")
            .and_then(|v| v.as_f64()),
        cwd: field("/payload/cwd"),
        started_at: field("/payload/timestamp")
            .and_then(|ts| ts.parse::<jiff::Timestamp>().ok())
            .and_then(|ts| SystemTime::try_from(ts).ok()),
        subagent,
        parent_id: field("/payload/source/subagent/thread_spawn/parent_thread_id")
            .filter(|id| !id.is_empty()),
    })
}

pub static PLUGIN: CodexPlugin = CodexPlugin;

pub struct CodexPlugin;

impl AgentPlugin for CodexPlugin {
    fn clear_caches(&self) {
        if let Ok(mut c) = TITLE_INDEXES.lock() {
            c.clear();
        }
        if let Ok(mut c) = SESSION_METAS.lock() {
            c.clear();
        }
        zstd_log::clear_cache();
    }

    fn id(&self) -> &'static str {
        "codex"
    }

    fn description(&self) -> &'static str {
        "Codex CLI (OpenAI)"
    }

    fn can_resume(&self) -> bool {
        true
    }
    fn prompts_in_session_json(&self) -> bool {
        true
    }
    fn prompts_per_jsonl_line(&self) -> bool {
        true
    }
    fn search_texts_in_session_json(&self) -> bool {
        true
    }
    fn search_texts_per_jsonl_line(&self) -> bool {
        true
    }

    fn can_detect_running(&self) -> bool {
        // Needs the kernel lock table (`/proc/locks`).
        cfg!(target_os = "linux")
    }

    fn can_memory(&self) -> bool {
        true
    }

    fn global_memory_sources(&self, home: &Path) -> Vec<MemorySource> {
        let base =
            crate::config::resolve_agent_base(self.id()).unwrap_or_else(|| home.join(".codex"));
        vec![
            MemorySource::new(&base, "AGENTS.md", MemoryKind::Instruction),
            MemorySource::new(&base, "AGENTS.override.md", MemoryKind::Instruction),
            MemorySource::new(&base, "memories/**/*.md", MemoryKind::Memory),
            MemorySource::new(&base, "skills/*/SKILL.md", MemoryKind::Skill),
        ]
    }

    fn project_memory_sources(&self, dir: &Path) -> Vec<MemorySource> {
        vec![MemorySource::new(
            dir,
            "AGENTS.override.md",
            MemoryKind::Instruction,
        )]
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative path of cwd)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".codex/sessions/**/*.jsonl",
            ".codex/archived_sessions/**/*.jsonl",
            ".codex/sessions/**/*.jsonl.zst",
            ".codex/archived_sessions/**/*.jsonl.zst",
        ]
    }

    /// A compressed rollout next to its plain copy is the same session; the
    /// plain one, which Codex may still be appending to, is listed.
    fn expand_sessions(&self, file: &Path) -> Option<Vec<(PathBuf, SystemTime)>> {
        (is_compressed(file) && file.with_extension("").exists()).then(Vec::new)
    }

    /// A compressed rollout is decoded, so it is loaded only for a
    /// full-text search.
    fn session_bytes(&self, path: &Path) -> Option<SessionBytes> {
        read_rollout(path).map(Log::into_session_bytes)
    }

    fn cheap_session_bytes(&self, path: &Path) -> bool {
        !is_compressed(path)
    }

    fn streams_raw_lines(&self, path: &Path) -> bool {
        is_compressed(path)
    }

    fn for_each_raw_line(&self, path: &Path, visit: &mut dyn FnMut(&[u8]) -> bool) {
        for_each_line(path, visit);
    }

    fn raw_content(&self, path: &Path) -> Option<String> {
        Some(String::from_utf8_lossy(&read_rollout(path)?).into_owned())
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.codex/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        for_each_line(path, |line| visit_message_line(line, visit));
    }

    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        for_each_message(data, visit);
    }

    fn iter_search_records(&self, path: &Path, visit: &mut RecordVisitor<'_>) {
        for_each_line(path, |line| {
            if line.len() < 2 {
                return true;
            }
            match serde_json::from_slice::<serde_json::Value>(line) {
                Ok(val) => visit.record(|emit| visit_search_value(&val, emit)),
                Err(_) => true,
            }
        });
    }

    fn iter_search_texts(&self, path: &Path, visit: &mut dyn FnMut(&str) -> bool) {
        for_each_line(path, |line| visit_search_line(line, visit));
    }

    fn iter_search_texts_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(&str) -> bool,
    ) {
        for_each_search_text(data, visit);
    }

    fn line_may_hold_search_texts(&self, line: &[u8]) -> bool {
        may_hold_search_text(line)
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        if crate::cloud::is_cache_path(path) {
            return None;
        }
        if let Some(cwd) = self.resolve_cwd(path, home) {
            Some(strip_home(&cwd, home))
        } else {
            RE_CODEX_SESSIONS
                .captures(&path.to_string_lossy())
                .map(|caps| caps[1].to_string())
                .or_else(|| Some("?".to_string()))
        }
    }

    fn resolve_date(&self, path: &Path, mtime: SystemTime) -> Option<String> {
        RE_CODEX_DATE
            .captures(&path.to_string_lossy())
            .map(|caps| format!("{} {}:{}", &caps[1], &caps[2], &caps[3]))
            .or_else(|| Some(format_mtime(mtime)))
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        session_meta(path)?.cwd.filter(|s| !s.is_empty())
    }

    fn session_created(&self, path: &Path) -> Option<SystemTime> {
        if crate::cloud::is_cache_path(path) {
            return crate::cloud::epoch(session_meta(path)?.cloud_created_at?);
        }
        // Compressing a rollout writes a new file, whose birth time is when
        // it was compressed.
        if is_compressed(path) {
            return session_meta(path)?.started_at;
        }
        path.metadata().ok()?.created().ok()
    }

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        if crate::cloud::is_cache_path(path) {
            return Some(format!(
                "[Cloud: latest turn] {}",
                session_meta(path)?.cloud_title.unwrap_or_default()
            ));
        }
        let session_id = session_meta(path)?.id?;
        latest_thread_name(path, &session_id)
    }

    fn is_archived(&self, path: &Path) -> bool {
        sessions_root(path).and_then(|dir| dir.file_name()) == Some("archived_sessions".as_ref())
    }

    fn is_subagent(&self, path: &Path) -> bool {
        session_meta(path).is_some_and(|meta| meta.subagent)
    }

    fn parent_session_id(&self, path: &Path) -> Option<String> {
        session_meta(path)?.parent_id
    }

    /// Each turn's settings, including the model, are logged as
    /// `turn_context`. A turn's model counts only once the model produced
    /// output in it, so a turn aborted before any reply adds nothing.
    fn resolve_models(&self, path: &Path) -> Option<super::common::ModelList> {
        let mut scan = ModelScan::default();
        if is_compressed(path) {
            for_each_line(path, |line| {
                if TURN_CONTEXT.find(line).is_some()
                    || (scan.pending.is_some() && RESPONSE_ITEM.find(line).is_some())
                {
                    scan.line(line);
                }
                true
            });
            return scan.models.non_empty();
        }
        let data = super::common::mmap_file(path)?;
        let mut turns = super::common::LinesWith::new(&data, b"\"turn_context\"");
        let mut items = super::common::LinesWith::new(&data, b"\"response_item\"");
        let mut from = 0;
        loop {
            // The next line naming a turn, or, while a turn waits for its
            // output, the next line naming a response item if that is first.
            let turn = turns.next_from(from);
            let item = scan.pending.as_ref().and_then(|_| items.next_from(from));
            let range = match (turn, item) {
                (Some(turn), Some(item)) if item.start < turn.start => item,
                (Some(turn), _) => turn,
                (None, Some(item)) => item,
                (None, None) => break,
            };
            from = range.end + 1;
            scan.line(&data[range]);
        }
        scan.models.non_empty()
    }

    // `codex resume <id>` also finds archived sessions, so they keep their id.
    fn session_id(&self, path: &Path, home: &Path) -> Option<String> {
        if crate::cloud::is_cache_path(path) {
            return path.file_stem()?.to_str().map(str::to_owned);
        }
        self.resolve_resume_id(path, home)
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        if crate::cloud::is_cache_path(path) {
            return None;
        }
        // A first line that is not JSON (empty or partly written file) has
        // no resumable id; only a readable line without one falls back to
        // the file name.
        if let Some(id) = session_meta(path)?.id.filter(|id| !id.is_empty()) {
            return Some(id);
        }

        let name = path.file_name()?.to_string_lossy();
        let stem = name.strip_suffix(".zst").unwrap_or(&name);
        let stem = stem.strip_suffix(".jsonl").unwrap_or(stem);
        RE_CODEX_ROLLOUT
            .captures(stem)
            .map(|caps| caps[1].to_string())
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        // An id starting with `-` would be read as an option by the CLI.
        let id = self
            .resolve_resume_id(path, home)
            .filter(|id| super::common::is_safe_cli_id(id))?;
        Some(vec!["codex".to_string(), "resume".to_string(), id])
    }

    fn running_sessions(&self) -> Vec<(String, Option<u32>)> {
        crate::config::resolve_agent_base("codex")
            .map(|base| {
                running_in(
                    &base.join("thread-writer-locks"),
                    &super::common::file_lock_holders(),
                )
            })
            .unwrap_or_default()
    }
}

/// Running sessions from `thread-writer-locks/<thread id>.lock`. Codex keeps
/// an exclusive `flock` on a thread's lock file while a process (the TUI's
/// app-server or `codex exec`) is writing to it; the files themselves stay
/// behind afterwards. The kernel lock table is read instead of probing the
/// lock, so `ah` never contends with Codex for it. The reported pid is the
/// lock holder, which for the TUI is the Codex app-server.
fn running_in(
    lock_dir: &Path,
    holders: &HashMap<(u64, u64, u64), u32>,
) -> Vec<(String, Option<u32>)> {
    if holders.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(lock_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let id = path
                .file_name()?
                .to_str()?
                .strip_suffix(".lock")
                .filter(|id| !id.is_empty() && !id.starts_with('.'))?
                .to_string();
            let pid = *holders.get(&super::common::file_lock_key(&path)?)?;
            Some((id, Some(pid)))
        })
        .collect()
}

static TURN_CONTEXT: LazyLock<memchr::memmem::Finder<'static>> =
    LazyLock::new(|| memchr::memmem::Finder::new(b"\"turn_context\""));

/// The models of a rollout's turns, read from its `turn_context` lines and
/// the `response_item` lines after them.
#[derive(Default)]
struct ModelScan {
    models: super::common::ModelList,
    /// The model of the latest turn, until that turn produces output.
    pending: Option<String>,
}

impl ModelScan {
    fn line(&mut self, line: &[u8]) {
        const PATHS: &[&[&str]] = &[
            &["type"],
            &["payload", "model"],
            &["payload", "type"],
            &["payload", "role"],
        ];
        let Some(val) = super::common::parse_json_paths(line, PATHS) else {
            return;
        };
        match val.get("type").and_then(|v| v.as_str()) {
            Some("turn_context") => {
                self.pending = val
                    .pointer("/payload/model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            Some("response_item") if is_model_output(val.get("payload")) => {
                if let Some(model) = self.pending.take() {
                    self.models.push(&model);
                }
            }
            _ => {}
        }
    }
}

/// Whether a `response_item` payload was produced by the model: an
/// assistant message, reasoning or a tool call (`*_call`). User and
/// developer messages, tool outputs, messages from other agents and local
/// records such as `ghost_snapshot` are not.
fn is_model_output(payload: Option<&serde_json::Value>) -> bool {
    let Some(payload) = payload else {
        return false;
    };
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("message") => payload.get("role").and_then(|v| v.as_str()) == Some("assistant"),
        Some("reasoning") => true,
        Some(kind) => kind.ends_with("_call"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "019c1dbb-cd96-70d3-baba-ef490967626c";

    fn write_session(codex_home: &Path, subdir: &str) -> PathBuf {
        let dir = codex_home.join(subdir).join("2026/09/27");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-09-27T10-00-00-{ID}.jsonl"));
        let meta =
            format!(r#"{{"type":"session_meta","payload":{{"id":"{ID}","cwd":"/tmp/proj"}}}}"#);
        fs::write(&path, format!("{meta}\n")).unwrap();
        path
    }

    fn write_index(codex_home: &Path, lines: &[(&str, &str)]) {
        let body: String = lines
            .iter()
            .map(|(id, name)| format!(r#"{{"id":"{id}","thread_name":"{name}"}}"#) + "\n")
            .collect();
        fs::write(codex_home.join("session_index.jsonl"), body).unwrap();
    }

    #[test]
    fn title_uses_latest_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let session = write_session(tmp.path(), "sessions");
        write_index(
            tmp.path(),
            &[
                (ID, "first name"),
                ("other-id", "unrelated"),
                (ID, "renamed"),
                ("other-id", "unrelated again"),
            ],
        );
        assert_eq!(
            PLUGIN.resolve_title(&session, Path::new("/nonexistent")),
            Some("renamed".to_string())
        );
    }

    #[test]
    fn title_reads_index_next_to_custom_codex_home() {
        // A CODEX_HOME-style directory that is not `~/.codex`.
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("my-codex");
        let session = write_session(&codex_home, "archived_sessions");
        write_index(&codex_home, &[(ID, "archived title")]);
        assert_eq!(
            PLUGIN.resolve_title(&session, tmp.path()),
            Some("archived title".to_string())
        );
    }

    #[test]
    fn title_falls_back_when_colocated_index_is_missing() {
        // e.g. an `extra_patterns` archive of session files without an index
        let tmp = tempfile::tempdir().unwrap();
        let session = write_session(&tmp.path().join("archive"), "sessions");
        let configured = tmp.path().join("codex-home");
        fs::create_dir_all(&configured).unwrap();
        write_index(&configured, &[(ID, "from configured base")]);
        assert_eq!(
            latest_thread_name_in(&session, ID, Some(&configured)),
            Some("from configured base".to_string())
        );
    }

    #[test]
    fn title_reflects_a_rename_after_the_index_was_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let session = write_session(tmp.path(), "sessions");
        write_index(tmp.path(), &[(ID, "before")]);
        let home = Path::new("/nonexistent");
        assert_eq!(
            PLUGIN.resolve_title(&session, home),
            Some("before".to_string())
        );
        write_index(tmp.path(), &[(ID, "before"), (ID, "after rename")]);
        assert_eq!(
            PLUGIN.resolve_title(&session, home),
            Some("after rename".to_string())
        );
    }

    #[test]
    fn title_ignores_lines_that_only_mention_the_id() {
        let tmp = tempfile::tempdir().unwrap();
        let session = write_session(tmp.path(), "sessions");
        write_index(tmp.path(), &[(ID, "real"), ("other-id", ID)]);
        assert_eq!(
            PLUGIN.resolve_title(&session, Path::new("/nonexistent")),
            Some("real".to_string())
        );
    }

    /// A rollout as Codex compresses it: `<name>.jsonl.zst`, one frame.
    fn write_compressed(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let path = dir.join(format!("{name}.jsonl.zst"));
        fs::write(&path, zstd::stream::encode_all(text.as_bytes(), 3).unwrap()).unwrap();
        path
    }

    #[test]
    fn compressed_rollouts_read_like_plain_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let meta = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{ID}","timestamp":"2026-09-27T01:00:00.500Z","cwd":"/tmp/proj"}}}}"#
        );
        let lines = [
            meta.as_str(),
            r#"{"type":"turn_context","payload":{"model":"gpt-x"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"compress me"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
        ];
        let dir = home.join(".codex/sessions/2026/09/27");
        let path = write_compressed(&dir, &format!("rollout-2026-09-27T10-00-00-{ID}"), &lines);
        assert!(!PLUGIN.cheap_session_bytes(&path));
        assert!(PLUGIN.cheap_session_bytes(&path.with_extension("")));
        assert_eq!(PLUGIN.expand_sessions(&path), None);
        assert_eq!(PLUGIN.session_id(&path, home).as_deref(), Some(ID));
        assert_eq!(
            PLUGIN.resolve_cwd(&path, home).as_deref(),
            Some("/tmp/proj")
        );
        assert_eq!(
            PLUGIN
                .resolve_date(&path, SystemTime::UNIX_EPOCH)
                .as_deref(),
            Some("2026-09-27 10:00")
        );
        // The birth time of a compressed file is when it was compressed.
        assert_eq!(
            PLUGIN.session_created(&path),
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_790_470_800_500))
        );
        let mut texts = Vec::new();
        PLUGIN.iter_messages(&path, &mut |m| {
            texts.push(m.text);
            true
        });
        assert_eq!(texts, ["compress me", "done"]);
        assert!(
            memchr::memmem::find(&PLUGIN.session_bytes(&path).unwrap(), b"compress me").is_some()
        );
        assert_eq!(
            PLUGIN.raw_content(&path).unwrap(),
            lines.map(|line| format!("{line}\n")).concat()
        );
        assert_eq!(
            PLUGIN.resolve_models(&path).map(|m| m.joined()).as_deref(),
            Some("gpt-x")
        );
        // Without an id in `session_meta`, the id comes from the file name.
        let path = write_compressed(
            &dir,
            &format!("rollout-2026-09-27T11-00-00-{ID}"),
            &[r#"{"type":"session_meta","payload":{}}"#],
        );
        assert_eq!(PLUGIN.resolve_resume_id(&path, home).as_deref(), Some(ID));
        // Damaged data has no metadata and no messages.
        fs::write(&path, b"not zstd").unwrap();
        PLUGIN.clear_caches();
        assert_eq!(PLUGIN.resolve_resume_id(&path, home), None);
        assert!(PLUGIN.session_bytes(&path).is_some_and(|b| b.is_empty()));
    }

    #[test]
    fn a_compressed_rollout_with_a_plain_copy_is_not_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = write_session(tmp.path(), "sessions");
        let name = plain.file_stem().unwrap().to_str().unwrap();
        let packed = write_compressed(plain.parent().unwrap(), name, &["{}"]);
        assert_eq!(PLUGIN.expand_sessions(&packed), Some(Vec::new()));
        assert_eq!(PLUGIN.expand_sessions(&plain), None);
    }

    #[test]
    fn option_like_ids_are_not_resumed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".codex/sessions/2026/09/27");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-09-27T10-00-00-{ID}.jsonl"));
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"--yolo\",\"cwd\":\"/tmp/p\"}}\n",
        )
        .unwrap();
        assert_eq!(PLUGIN.resume_args(&path, tmp.path()), None);
    }

    #[test]
    fn archived_sessions_keep_their_resume_id() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let active = write_session(&home.join(".codex"), "sessions");
        let archived = write_session(&home.join(".codex"), "archived_sessions");
        assert!(!PLUGIN.is_archived(&active));
        assert!(PLUGIN.is_archived(&archived));
        assert_eq!(
            PLUGIN.resolve_resume_id(&archived, home).as_deref(),
            Some(ID)
        );
        assert_eq!(
            PLUGIN.resume_args(&archived, home),
            Some(vec![
                "codex".to_string(),
                "resume".to_string(),
                ID.to_string()
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn running_in_maps_held_lock_files_to_threads() {
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("0199-held.lock");
        let free = dir.path().join("0199-free.lock");
        let coord = dir.path().join(".coordination.lock");
        for p in [&held, &free, &coord] {
            fs::write(p, "").unwrap();
        }
        let mut holders = HashMap::new();
        holders.insert(super::super::common::file_lock_key(&held).unwrap(), 4242);
        holders.insert(super::super::common::file_lock_key(&coord).unwrap(), 4243);
        assert_eq!(
            running_in(dir.path(), &holders),
            vec![("0199-held".to_string(), Some(4242))]
        );
        assert!(running_in(dir.path(), &HashMap::new()).is_empty());
    }

    const ROLLOUT: &str = concat!(
        r#"{"type":"session_meta","payload":{"id":"x","cwd":"/tmp/p","base_instructions":{"text":"user assistant"}}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"user_message","message":"not a response item"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>"}]}}"#,
        "\n",
        r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}"##,
        "\n",
        r#"{"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix the bug"}]}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"role\":\"user\"}"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
        "\n",
    );

    fn collect(visit_all: impl FnOnce(&mut dyn FnMut(Message) -> bool)) -> Vec<Message> {
        let mut messages = Vec::new();
        visit_all(&mut |m| {
            messages.push(m);
            true
        });
        messages
    }

    #[test]
    fn messages_skip_non_message_lines_and_injected_prompts() {
        let messages = collect(|visit| for_each_message(ROLLOUT.as_bytes(), visit));
        assert_eq!(
            messages,
            vec![
                Message::user("fix the bug".to_string()),
                Message::assistant("done".to_string()),
            ]
        );
    }

    #[test]
    fn messages_from_file_and_bytes_agree() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        fs::write(&path, ROLLOUT).unwrap();
        let from_file = collect(|visit| PLUGIN.iter_messages(&path, visit));
        let from_bytes =
            collect(|visit| PLUGIN.iter_messages_from_bytes(&path, ROLLOUT.as_bytes(), visit));
        assert_eq!(from_file, from_bytes);
        assert_eq!(from_file.len(), 2);
    }

    #[test]
    fn session_meta_is_reread_when_the_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        fs::write(&path, ROLLOUT).unwrap();
        assert_eq!(
            PLUGIN.resolve_cwd(&path, tmp.path()).as_deref(),
            Some("/tmp/p")
        );
        assert_eq!(
            PLUGIN.resolve_resume_id(&path, tmp.path()).as_deref(),
            Some("x")
        );

        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"y","cwd":"/tmp/q"}}"#.to_string() + "\n",
        )
        .unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            PLUGIN.resolve_cwd(&path, tmp.path()).as_deref(),
            Some("/tmp/q")
        );
        assert_eq!(
            PLUGIN.resolve_resume_id(&path, tmp.path()).as_deref(),
            Some("y")
        );
    }

    #[test]
    fn session_meta_is_reread_when_only_the_size_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        let fixed = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        let write = |body: &str| {
            fs::write(&path, body).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(fixed)
                .unwrap();
        };
        write("{\"payload\":{\"id\":\"a\"}}\n");
        assert_eq!(
            PLUGIN.resolve_resume_id(&path, tmp.path()).as_deref(),
            Some("a")
        );
        write("{\"payload\":{\"id\":\"longer\"}}\n");
        assert_eq!(
            PLUGIN.resolve_resume_id(&path, tmp.path()).as_deref(),
            Some("longer")
        );
    }

    #[test]
    fn fields_are_read_independently_like_json_pointers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(format!("rollout-2026-09-27T10-00-00-{ID}.jsonl"));
        fs::write(&path, "{\"payload\":{\"id\":123,\"cwd\":\"/tmp/p\"}}\n").unwrap();
        assert_eq!(
            PLUGIN.resolve_cwd(&path, tmp.path()).as_deref(),
            Some("/tmp/p")
        );
        // A readable line without a string id falls back to the file name.
        assert_eq!(
            PLUGIN.resolve_resume_id(&path, tmp.path()).as_deref(),
            Some(ID)
        );

        fs::write(
            &path,
            "{\"payload\":{\"id\":\"real\",\"cwd\":1,\"cwd\":\"/tmp/q\"}}\n",
        )
        .unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            PLUGIN.resolve_resume_id(&path, tmp.path()).as_deref(),
            Some("real")
        );
        assert_eq!(
            PLUGIN.resolve_cwd(&path, tmp.path()).as_deref(),
            Some("/tmp/q")
        );
    }

    #[test]
    fn unreadable_first_line_has_no_resume_id() {
        let tmp = tempfile::tempdir().unwrap();
        for body in ["", "{broken\n"] {
            let path = tmp
                .path()
                .join(format!("rollout-2026-09-27T10-00-00-{ID}.jsonl"));
            fs::write(&path, body).unwrap();
            let later = SystemTime::now() + std::time::Duration::from_secs(body.len() as u64 + 1);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(later)
                .unwrap();
            assert_eq!(PLUGIN.resolve_resume_id(&path, tmp.path()), None);
            assert_eq!(PLUGIN.resolve_cwd(&path, tmp.path()), None);
        }
    }

    #[test]
    fn model_comes_from_turn_context() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_session(tmp.path(), "sessions");
        let mut body = fs::read_to_string(&path).unwrap();
        body.push_str(
            r#"{"type":"turn_context","payload":{"model":"gpt-6-astra","effort":"high"}}"#,
        );
        body.push('\n');
        body.push_str(r#"{"type":"event_msg","payload":{"type":"x","model":"not-this"}}"#);
        body.push('\n');
        body.push('\n');
        body.push_str(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#,
        );
        body.push('\n');
        body.push_str(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell"}}"#,
        );
        body.push('\n');
        // Aborted before any reply: user input, a local undo snapshot, a
        // message from another agent and tool output only.
        body.push_str(r#"{"type":"turn_context","payload":{"model":"gpt-aborted"}}"#);
        body.push('\n');
        body.push_str(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#,
        );
        body.push('\n');
        body.push_str(r#"{"type":"response_item","payload":{"type":"ghost_snapshot"}}"#);
        body.push('\n');
        body.push_str(r#"{"type":"response_item","payload":{"type":"agent_message"}}"#);
        body.push('\n');
        body.push_str(
            r#"{"type":"response_item","payload":{"type":"function_call_output","output":"x"}}"#,
        );
        body.push('\n');
        body.push_str(r#"{"type":"turn_context","payload":{"model":"gpt-6-sol"}}"#);
        body.push('\n');
        body.push_str(r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#);
        fs::write(&path, body).unwrap();
        assert_eq!(
            PLUGIN.resolve_models(&path).map(|m| m.joined()).as_deref(),
            Some("gpt-6-astra,gpt-6-sol")
        );
    }
}

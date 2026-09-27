use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use chrono::{Local, NaiveDateTime, TimeZone, Utc};
use regex::Regex;
use sha2::{Digest, Sha256};

use super::AgentPlugin;
use super::Message;
use super::MessageRole;
use super::common::first_text_part;
use super::common::format_mtime;
use super::common::mmap_file;
use super::common::{visit_all_strings, visit_text_parts, visit_tool_call, visit_tool_output};
use super::{MemoryKind, MemorySource};

/// Project directory of a session file: `tmp/{project}/chats/session-*` or
/// `tmp/{project}/logs.json`. Anchored to the end of the path so that a
/// project named `tmp` (cwd `/tmp`) does not capture `chats`.
/// Parent session id of a subagent log (`chats/<parent id>/<id>.json` or
/// `.jsonl`). Main session files are always named `session-*` and subagent
/// logs never are, so a project named `chats` is not mistaken for a parent
/// directory, and extra locations (`extra_patterns`) keep the same layout.
fn subagent_parent(path: &Path) -> Option<&str> {
    if path.file_name()?.to_str()?.starts_with("session-") {
        return None;
    }
    let dir = path.parent()?;
    if dir.parent()?.file_name()? != "chats" {
        return None;
    }
    dir.file_name()?.to_str()
}

static RE_GEMINI_TMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/tmp/([^/]+)/(?:chats/(?:[^/]+/)?[^/]+|logs\.json)$").unwrap());
/// Session start time in the file name, written in UTC by Gemini CLI
/// (`new Date().toISOString()`).
static RE_GEMINI_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"session-(\d{4}-\d{2}-\d{2}T\d{2}-\d{2})(?:-[^/]*)?\.jsonl?$").unwrap()
});

/// Gemini CLI's home (`GEMINI_CLI_HOME` or `~/.gemini`).
fn gemini_base(home: &Path) -> PathBuf {
    crate::config::resolve_agent_base("gemini").unwrap_or_else(|| home.join(".gemini"))
}

/// Context file names Gemini CLI loads (`GEMINI.md` unless `settings.json`
/// sets `context.fileName` or the older `contextFileName`, either a string
/// or a list). Names are escaped for use in glob patterns.
fn context_file_names(base: &Path) -> Vec<String> {
    let settings = fs::read_to_string(base.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let configured = settings.as_ref().and_then(|s| {
        s.pointer("/context/fileName")
            .or_else(|| s.get("contextFileName"))
    });
    let mut names: Vec<String> = match configured {
        Some(serde_json::Value::String(name)) => vec![name.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    names.retain(|n| !n.is_empty() && !n.contains('/'));
    if names.is_empty() {
        names.push("GEMINI.md".to_string());
    }
    names.iter().map(|n| glob::Pattern::escape(n)).collect()
}

/// Whether `path` is listed as a Gemini session on its own: it is attributed
/// to an active agent parsed by this plugin, and such an agent globs it. A
/// custom agent whose patterns only match `session-*.json` keeps its legacy
/// files instead. Patterns are compared as written, so non-canonical
/// spellings such as `//` or `/./` are not recognised.
fn collected_as_gemini(path: &Path) -> bool {
    if crate::config::find_plugin_for_path(path).id() != PLUGIN.id() {
        return false;
    }
    let options = glob::MatchOptions {
        require_literal_separator: true,
        ..glob::MatchOptions::new()
    };
    crate::config::active_agents()
        .filter(|agent| agent.plugin.id() == PLUGIN.id())
        .flat_map(|agent| &agent.glob_patterns)
        .filter_map(|pattern| glob::Pattern::new(pattern).ok())
        .any(|pattern| pattern.matches_path_with(path, options))
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

/// Look up the project directory name in `{base}/projects.json`
/// (`{"projects": {"<cwd>": "<project>"}}`) and return the matching cwd.
fn resolve_cwd_from_projects_json(gemini_base: &Path, project: &str) -> Option<String> {
    let content = fs::read_to_string(gemini_base.join("projects.json")).ok()?;
    let root: serde_json::Value = serde_json::from_str(&content).ok()?;
    root.get("projects")?
        .as_object()?
        .iter()
        .find(|(_, name)| name.as_str() == Some(project))
        .map(|(cwd, _)| cwd.clone())
}

/// The session metadata line (also the whole record in the one-line legacy
/// form), as recognised by Gemini CLI.
fn is_metadata_record(record: &serde_json::Value) -> bool {
    record.get("sessionId").is_some_and(|v| v.is_string())
        && record.get("projectHash").is_some_and(|v| v.is_string())
}

/// Messages reconstructed from a `session-*.jsonl` file.
///
/// The file is an append-only log: the first line holds session metadata,
/// message records are upserted by `id`, `{"$set": {...}}` updates metadata
/// (a `messages` array replaces the whole conversation), and
/// `{"$rewindTo": "<id>"}` drops that message and everything after it.
#[derive(Default)]
struct JsonlSession {
    messages: Vec<serde_json::Value>,
    index: HashMap<String, usize>,
}

impl JsonlSession {
    fn upsert(&mut self, message: serde_json::Value) {
        let Some(id) = message.get("id").and_then(|v| v.as_str()) else {
            return;
        };
        match self.index.get(id) {
            Some(&i) => self.messages[i] = message,
            None => {
                self.index.insert(id.to_string(), self.messages.len());
                self.messages.push(message);
            }
        }
    }

    fn clear(&mut self) {
        self.messages.clear();
        self.index.clear();
    }

    fn replay(data: &[u8]) -> Self {
        let mut session = JsonlSession::default();
        for line in data.split(|&b| b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Ok(mut record) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(target) = record.get("$rewindTo").and_then(|v| v.as_str()) {
                match session.index.get(target).copied() {
                    Some(cut) => {
                        session.messages.truncate(cut);
                        session.index.retain(|_, i| *i < cut);
                    }
                    None => session.clear(),
                }
            } else if record.get("id").is_some_and(|v| v.is_string()) {
                session.upsert(record);
            } else if let Some(set) = record.get_mut("$set").and_then(|v| v.as_object_mut()) {
                if let Some(serde_json::Value::Array(messages)) = set.remove("messages") {
                    session.clear();
                    messages.into_iter().for_each(|m| session.upsert(m));
                }
            } else if is_metadata_record(&record) {
                if let Some(serde_json::Value::Array(messages)) =
                    record.as_object_mut().and_then(|o| o.remove("messages"))
                {
                    messages.into_iter().for_each(|m| session.upsert(m));
                }
            }
        }
        session
    }
}

/// Whether a `session-*.jsonl` file has a session metadata line with a
/// non-empty id. Stops at the first one (normally the first line).
fn has_metadata_line(data: &[u8]) -> bool {
    data.split(|&b| b == b'\n')
        .filter(|line| memchr::memmem::find(line, b"\"sessionId\"").is_some())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .any(|record| {
            is_metadata_record(&record)
                && record.get("id").is_none()
                && record["sessionId"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
        })
}

/// Session id of a `session-*.jsonl` file without replaying its messages:
/// the last metadata line or `$set` that carries a `sessionId`, classified
/// the same way as `JsonlSession::replay`. Only lines mentioning
/// `"sessionId"` are parsed, so large message records are skipped cheaply.
fn jsonl_session_id(data: &[u8]) -> Option<String> {
    let mut session_id = None;
    for line in data.split(|&b| b == b'\n') {
        if memchr::memmem::find(line, b"\"sessionId\"").is_none() {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let meta = if record.get("$rewindTo").is_some_and(|v| v.is_string())
            || record.get("id").is_some_and(|v| v.is_string())
        {
            None
        } else if let Some(set) = record.get("$set").filter(|v| v.is_object()) {
            Some(set)
        } else if is_metadata_record(&record) {
            Some(&record)
        } else {
            None
        };
        if let Some(id) = meta
            .and_then(|m| m.get("sessionId"))
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
        {
            session_id = Some(id.to_string());
        }
    }
    session_id
}

/// Derive the Gemini base directory by finding `tmp/{project}` or `history/{project}` in the path.
/// Returns the parent of `tmp`/`history` (i.e., the Gemini home).
fn derive_gemini_base<'a>(path: &'a Path, project: &str) -> Option<&'a Path> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.file_name().and_then(|s| s.to_str()) == Some(project) {
            if let Some(parent) = dir.parent() {
                let parent_name = parent.file_name().and_then(|s| s.to_str());
                if matches!(parent_name, Some("tmp" | "history")) {
                    return parent.parent();
                }
            }
        }
        current = dir.parent();
    }
    None
}

/// Try to resolve a SHA-256 hash to a known directory path.
/// Checks home dir and its immediate children.
fn resolve_hash_to_path(hash: &str, home: &Path) -> Option<String> {
    fn sha256_hex(s: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(s.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    // Check home directory itself
    let home_str = home.to_string_lossy();
    if sha256_hex(&home_str) == hash {
        return Some(home_str.to_string());
    }

    // Check immediate children of home
    if let Ok(entries) = fs::read_dir(home) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let path_str = path.to_string_lossy();
                if sha256_hex(&path_str) == hash {
                    return Some(path_str.to_string());
                }
            }
        }
    }

    None
}

/// Convert one Gemini message record into a transcript message and visit it.
/// Returns `false` to stop iteration.
fn visit_message(val: &serde_json::Value, visit: &mut dyn FnMut(Message) -> bool) -> bool {
    match val.get("type").and_then(|v| v.as_str()) {
        Some("user") => {
            // chats/session-*.json(l) uses "content", logs.json uses "message"
            let text = val
                .get("content")
                .and_then(first_text_part)
                .or_else(|| val.get("message").and_then(|v| v.as_str()));
            if let Some(text) = text {
                if !text.starts_with('<') {
                    return visit(Message::user(text.to_string()));
                }
            }
        }
        Some("gemini") => {
            if let Some(text) = val.get("content").and_then(assistant_text) {
                return visit(Message::assistant(text));
            }
        }
        _ => {}
    }
    true
}

/// Search texts of one Gemini message record: the user prompt, each answer
/// part of a turn (visited separately, so each is one JSON string), and its
/// tool calls with their arguments and results. Returns `false` to stop.
fn visit_search_record(val: &serde_json::Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match val.get("type").and_then(|v| v.as_str()) {
        // every prompt part (injected context `<...>` is skipped), and tool
        // results sent back as `functionResponse` parts of a user turn
        Some("user") => {
            let prompts_ok = {
                let mut visit_prompt = |text: &str| text.starts_with('<') || visit(text);
                val.get("content")
                    .is_none_or(|content| visit_text_parts(content, &mut visit_prompt))
                    && val
                        .get("message")
                        .and_then(|v| v.as_str())
                        .is_none_or(&mut visit_prompt)
            };
            prompts_ok
                && val
                    .get("content")
                    .and_then(|v| v.as_array())
                    .is_none_or(|parts| {
                        parts.iter().all(|part| {
                            ["functionCall", "functionResponse"].iter().all(|key| {
                                part.get(*key).is_none_or(|v| visit_function_part(v, visit))
                            })
                        })
                    })
        }
        Some("gemini") => {
            let content_ok = match val.get("content") {
                Some(serde_json::Value::String(text)) => visit(text),
                Some(serde_json::Value::Array(parts)) => parts.iter().all(|part| {
                    if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                        return true;
                    }
                    part.get("text")
                        .and_then(|v| v.as_str())
                        .is_none_or(&mut *visit)
                        && ["functionCall", "functionResponse"]
                            .iter()
                            .all(|key| part.get(key).is_none_or(|v| visit_function_part(v, visit)))
                }),
                _ => true,
            };
            content_ok
                && val
                    .get("toolCalls")
                    .and_then(|v| v.as_array())
                    .is_none_or(|calls| {
                        calls.iter().all(|call| {
                            visit_tool_call(call.get("name"), call.get("args"), visit)
                                && call
                                    .get("result")
                                    .is_none_or(|result| visit_tool_result(result, visit))
                        })
                    })
        }
        _ => true,
    }
}

/// A tool call's `result`: `functionResponse` parts or other output.
fn visit_tool_result(result: &serde_json::Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    let Some(items) = result.as_array() else {
        return visit_tool_output(result, visit);
    };
    items.iter().all(|item| match item.get("functionResponse") {
        Some(response) => visit_function_part(response, visit),
        None => visit_tool_output(item, visit),
    })
}

/// A `functionCall` (`name`, `args`) or `functionResponse` (`name`,
/// `response`) part; its `id` is not searched.
fn visit_function_part(part: &serde_json::Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    part.get("name")
        .and_then(|v| v.as_str())
        .is_none_or(&mut *visit)
        && ["args", "response"]
            .iter()
            .all(|key| part.get(*key).is_none_or(|v| visit_all_strings(v, visit)))
}

/// Answer text of a Gemini turn. `content` is a string, or (after a
/// `$set.messages` checkpoint) a Parts array mixing thought summaries
/// (`"thought": true`), function calls and answer text; only the answer
/// text is kept. Tool-call-only and thought-only turns yield `None`.
fn assistant_text(content: &serde_json::Value) -> Option<String> {
    let text: String = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("thought").and_then(|v| v.as_bool()) != Some(true))
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .collect(),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Local time of a session file name's UTC start time (`2026-06-11T02-44`).
fn file_time_in<Tz: TimeZone>(utc: &str, tz: &Tz) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let utc = NaiveDateTime::parse_from_str(utc, "%Y-%m-%dT%H-%M").ok()?;
    Some(
        Utc.from_utc_datetime(&utc)
            .with_timezone(tz)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
    )
}

/// Title of a `session-*.jsonl` file: the first user prompt as Gemini CLI
/// computes it while streaming the log, which stops reading early instead
/// of replaying large logs.
fn jsonl_first_prompt(data: &[u8]) -> Option<String> {
    let mut first = None;
    for line in data.split(|&b| b == b'\n') {
        if memchr::memmem::find(line, b"\"user\"").is_none() {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let candidates: Vec<&serde_json::Value> =
            if record.get("$rewindTo").is_some_and(|v| v.is_string()) {
                Vec::new()
            } else if record.get("id").is_some_and(|v| v.is_string()) {
                vec![&record]
            } else {
                record
                    .get("$set")
                    .filter(|v| v.is_object())
                    .or_else(|| is_metadata_record(&record).then_some(&record))
                    .and_then(|m| m.get("messages"))
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().collect())
                    .unwrap_or_default()
            };
        for message in candidates {
            if message.get("id").is_some_and(|v| v.is_string()) {
                visit_message(message, &mut |m| {
                    if m.role == MessageRole::User {
                        first = Some(m.text);
                    }
                    first.is_none()
                });
            }
            if first.is_some() {
                return first;
            }
        }
    }
    None
}

pub static PLUGIN: GeminiPlugin = GeminiPlugin;

pub struct GeminiPlugin;

impl GeminiPlugin {
    fn for_each_root_message(path: &Path, visit: impl FnMut(&serde_json::Value) -> bool) {
        if let Some(mmap) = mmap_file(path) {
            Self::for_each_root_message_bytes(path, &mmap, visit);
        }
    }

    fn for_each_root_message_bytes(
        path: &Path,
        data: &[u8],
        mut visit: impl FnMut(&serde_json::Value) -> bool,
    ) {
        if is_jsonl(path) {
            for message in &JsonlSession::replay(data).messages {
                if !visit(message) {
                    return;
                }
            }
            return;
        }

        let root = match serde_json::from_slice::<serde_json::Value>(data) {
            Ok(root) => root,
            Err(_) => return,
        };

        if let Some(messages) = root.get("messages").and_then(|v| v.as_array()) {
            for message in messages {
                if !visit(message) {
                    return;
                }
            }
            return;
        }

        if let Some(items) = root.as_array() {
            for item in items {
                if !visit(item) {
                    return;
                }
            }
        }
    }
}

impl AgentPlugin for GeminiPlugin {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn description(&self) -> &'static str {
        "Gemini CLI (Google)"
    }

    fn can_resume(&self) -> bool {
        true
    }
    fn prompts_in_session_json(&self) -> bool {
        true
    }

    fn can_memory(&self) -> bool {
        true
    }

    fn global_memory_sources(&self, home: &Path) -> Vec<MemorySource> {
        let base = gemini_base(home);
        context_file_names(&base)
            .iter()
            .map(|name| MemorySource::new(&base, name, MemoryKind::Instruction))
            .collect()
    }

    fn project_memory_sources(&self, dir: &Path) -> Vec<MemorySource> {
        let base = gemini_base(&super::common::canonical_home());
        context_file_names(&base)
            .iter()
            // A project AGENTS.md is listed once, as shared.
            .filter(|name| name.as_str() != "AGENTS.md")
            .map(|name| MemorySource::new(dir, name, MemoryKind::Instruction))
            .collect()
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: directory name from .gemini/tmp/, cwd from .project_root)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".gemini/tmp/*/chats/session-*.json",
            ".gemini/tmp/*/chats/session-*.jsonl",
            ".gemini/tmp/*/logs.json",
        ]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.gemini/"]
    }

    fn subagent_glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".gemini/tmp/*/chats/*/*.json",
            ".gemini/tmp/*/chats/*/*.jsonl",
        ]
    }

    fn parent_session_id(&self, path: &Path) -> Option<String> {
        subagent_parent(path).map(str::to_string)
    }

    fn is_secondary_record(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "logs.json")
    }

    /// Resuming a legacy `session-*.json` makes Gemini CLI write the whole
    /// conversation to a sibling `session-*.jsonl` and leave the `.json`
    /// behind; list only the `.jsonl` copy.
    fn expand_sessions(&self, file: &Path) -> Option<Vec<(PathBuf, SystemTime)>> {
        let name = file.file_name()?.to_str()?;
        if !name.starts_with("session-") || !name.ends_with(".json") {
            return None;
        }
        let jsonl = file.with_file_name(format!("{name}l"));
        // An empty or unparseable copy must not hide the legacy session.
        let jsonl_readable = || mmap_file(&jsonl).is_some_and(|data| has_metadata_line(&data));
        if jsonl.is_file() && collected_as_gemini(&jsonl) && jsonl_readable() {
            return Some(Vec::new());
        }
        None
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        Self::for_each_root_message(path, |val| visit_message(val, visit));
    }

    fn iter_messages_from_bytes(
        &self,
        path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        Self::for_each_root_message_bytes(path, data, |val| visit_message(val, visit));
    }

    fn iter_search_texts(&self, path: &Path, visit: &mut dyn FnMut(&str) -> bool) {
        Self::for_each_root_message(path, |val| visit_search_record(val, visit));
    }

    fn iter_search_texts_from_bytes(
        &self,
        path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(&str) -> bool,
    ) {
        Self::for_each_root_message_bytes(path, data, |val| visit_search_record(val, visit));
    }

    fn search_texts_in_session_json(&self) -> bool {
        true
    }

    fn resolve_project(&self, path: &Path, _home: &Path) -> Option<String> {
        RE_GEMINI_TMP
            .captures(&path.to_string_lossy())
            .map(|caps| caps[1].to_string())
            .or_else(|| Some("?".to_string()))
    }

    fn resolve_date(&self, path: &Path, mtime: SystemTime) -> Option<String> {
        RE_GEMINI_DATE
            .captures(&path.to_string_lossy())
            .and_then(|caps| file_time_in(&caps[1], &Local))
            .or_else(|| Some(format_mtime(mtime)))
    }

    fn resolve_title(&self, path: &Path, home: &Path) -> Option<String> {
        if !is_jsonl(path) {
            return None;
        }
        self.resolve_title_from_mmap(path, home, &mmap_file(path)?)
    }

    fn resolve_title_from_mmap(&self, path: &Path, _home: &Path, mmap: &[u8]) -> Option<String> {
        // A rewound log may have dropped its first prompt; leave the title to
        // the replayed first prompt so that title and transcript agree.
        // `$set.messages` checkpoints are not excluded: they are in about half
        // of all logs and normally rebuild the same history, and Gemini CLI itself
        // titles sessions with the first prompt seen while streaming.
        if !is_jsonl(path) || memchr::memmem::find(mmap, b"\"$rewindTo\"").is_some() {
            return None;
        }
        // Same shape as the first-prompt fallback: first line only
        jsonl_first_prompt(mmap).map(|p| p.lines().next().unwrap_or_default().to_string())
    }

    fn resolve_cwd(&self, path: &Path, home: &Path) -> Option<String> {
        let path_str = path.to_string_lossy();
        let project = RE_GEMINI_TMP.captures(&path_str)?.get(1)?.as_str();

        // Derive gemini base from session file path (supports GEMINI_CLI_HOME override).
        // Walk up from the file until we find {tmp,history}/{project} and take the parent of tmp/history.
        // This handles both:
        //   {base}/tmp/{project}/chats/session-*.json(l)
        //   {base}/tmp/{project}/logs.json
        let gemini_base = derive_gemini_base(path, project)
            .or_else(|| path.parent()?.parent()?.parent()?.parent())?;

        // Try .project_root in tmp/ first, then history/
        for dir in &["tmp", "history"] {
            let root_file = gemini_base.join(format!("{}/{}/.project_root", dir, project));
            if let Ok(content) = fs::read_to_string(&root_file) {
                let trimmed = content.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }

        if let Some(cwd) = resolve_cwd_from_projects_json(gemini_base, project) {
            return Some(cwd);
        }

        // Fallback: try home-based path (legacy)
        for dir in &["tmp", "history"] {
            let root_file = home.join(format!(".gemini/{}/{}/.project_root", dir, project));
            if let Ok(content) = fs::read_to_string(&root_file) {
                let trimmed = content.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }

        // Fallback: if project looks like a SHA-256 hash, try matching known paths
        if project.len() == 64 && project.chars().all(|c| c.is_ascii_hexdigit()) {
            return resolve_hash_to_path(project, home);
        }
        None
    }

    // `gemini --resume` lists only top-level sessions, not subagent logs.
    fn resolve_resume_id(&self, path: &Path, home: &Path) -> Option<String> {
        if subagent_parent(path).is_some() {
            return None;
        }
        self.session_id(path, home)
    }

    fn session_id(&self, path: &Path, _home: &Path) -> Option<String> {
        // A subagent log is named after its session id; skip reading it
        // (id lookups visit every subagent log, some tens of MB).
        if subagent_parent(path).is_some() {
            return path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string);
        }
        let mmap = mmap_file(path)?;
        if is_jsonl(path) {
            return jsonl_session_id(&mmap);
        }
        let root = serde_json::from_slice::<serde_json::Value>(&mmap).ok()?;
        // chats/session-*.json: root is an object with "sessionId"
        if let Some(id) = root.get("sessionId").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        // logs.json: root is an array of objects, take the last sessionId
        if let Some(arr) = root.as_array() {
            for item in arr.iter().rev() {
                if let Some(id) = item.get("sessionId").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        return Some(id.to_string());
                    }
                }
            }
        }
        None
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["gemini".to_string(), "--resume".to_string(), id])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_logs_name_their_parent_and_are_not_resumable() {
        let tmp = tempfile::tempdir().unwrap();
        let chats = tmp.path().join(".gemini/tmp/proj/chats");
        let child = chats.join("parent-uuid/zut36a.jsonl");
        std::fs::create_dir_all(child.parent().unwrap()).unwrap();
        std::fs::write(
            &child,
            "{\"sessionId\":\"zut36a\",\"projectHash\":\"h\",\"kind\":\"subagent\"}\n",
        )
        .unwrap();
        let home = tmp.path();
        assert!(PLUGIN.is_subagent(&child));
        assert_eq!(
            PLUGIN.parent_session_id(&child).as_deref(),
            Some("parent-uuid")
        );
        assert_eq!(PLUGIN.session_id(&child, home).as_deref(), Some("zut36a"));
        assert_eq!(PLUGIN.resume_args(&child, home), None);
        assert_eq!(
            PLUGIN.resolve_project(&child, home).as_deref(),
            Some("proj")
        );
        let main = chats.join("session-2026-06-11T02-44-acc93477.jsonl");
        assert!(!PLUGIN.is_subagent(&main));
        // A base directory named `tmp` with a project named `chats`.
        let nested = Path::new("/x/tmp/tmp/chats/chats/session-2026-06-11T02-44-acc93477.json");
        assert!(!PLUGIN.is_subagent(nested));
        // Extra locations keep the chats/<parent>/<id> layout.
        let extra = Path::new("/x/gemarchive/proj/chats/parent-uuid/childabc.jsonl");
        assert_eq!(
            PLUGIN.parent_session_id(extra).as_deref(),
            Some("parent-uuid")
        );
    }

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn messages(path: &Path) -> Vec<(MessageRole, String)> {
        let mut out = Vec::new();
        PLUGIN.iter_messages(path, &mut |m| {
            out.push((m.role, m.text));
            true
        });
        out
    }

    #[test]
    fn jsonl_replays_upserts_rewinds_and_checkpoints() {
        use MessageRole::{Assistant, User};
        assert_eq!(
            messages(&fixture_path("gemini_session.jsonl")),
            vec![
                (User, "draft the migration plan".to_string()),
                (Assistant, "Here is the plan.".to_string()),
                (User, "add a rollback step".to_string()),
                (Assistant, "Rollback step added.".to_string()),
                (Assistant, "Final answer.".to_string()),
            ]
        );
    }

    #[test]
    fn jsonl_title_only_for_jsonl_without_rewind() {
        // The fixture contains a `$rewindTo`, so the title comes from replay
        let path = fixture_path("gemini_session.jsonl");
        assert_eq!(PLUGIN.resolve_title(&path, Path::new("/")), None);
        let legacy = fixture_path("gemini_session.json");
        assert_eq!(PLUGIN.resolve_title(&legacy, Path::new("/")), None);
    }

    #[test]
    fn jsonl_checkpoint_replaces_messages() {
        let checkpoint = r#"{"sessionId":"s","projectHash":"h"}
{"id":"a","type":"user","content":[{"text":"old"}]}
{"$set":{"messages":[{"id":"b","type":"user","content":[{"text":"new"}]}]}}
{"id":"c","type":"user","content":[{"text":"next"}]}
"#;
        let ids = |data: &str| -> Vec<String> {
            JsonlSession::replay(data.as_bytes())
                .messages
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(ids(checkpoint), ["b", "c"]);
        // Rewinding to an unknown id clears the conversation
        let reset = format!(
            "{checkpoint}{}\n{}\n",
            r#"{"$rewindTo":"missing"}"#, r#"{"id":"d","type":"user","content":"after reset"}"#
        );
        assert_eq!(ids(&reset), ["d"]);
    }

    #[test]
    fn jsonl_session_id_prefers_latest_set() {
        let path = fixture_path("gemini_session.jsonl");
        assert_eq!(
            PLUGIN.resolve_resume_id(&path, Path::new("/")).as_deref(),
            Some("gemini-jsonl-001")
        );
        let data = br#"{"sessionId":"first","projectHash":"h"}
{"id":"m","type":"user","content":"mentions \"sessionId\""}
{"$set":{"sessionId":"second"}}
"#;
        assert_eq!(jsonl_session_id(data).as_deref(), Some("second"));
        // Blank or corrupt leading lines and whitespace inside records
        let data = b"\n{broken\n { \"sessionId\" : \"late\", \"projectHash\": \"h\" }\n";
        assert_eq!(jsonl_session_id(data).as_deref(), Some("late"));
        let data =
            b"{\"sessionId\":\"a\",\"projectHash\":\"h\"}\n { \"$set\": {\"sessionId\":\"b\"}}\n";
        assert_eq!(jsonl_session_id(data).as_deref(), Some("b"));
    }

    #[test]
    fn jsonl_title_defers_to_replay_after_rewind() {
        let data = br#"{"sessionId":"s","projectHash":"h"}
{"id":"u1","type":"user","content":[{"text":"typo promt"}]}
{"id":"g1","type":"gemini","content":"?"}
{"$rewindTo":"u1"}
{"id":"u2","type":"user","content":[{"text":"corrected prompt\nsecond line"}]}
"#;
        let path = Path::new("/h/.gemini/tmp/p/chats/session-2026-06-01T00-00-abcd1234.jsonl");
        assert_eq!(
            PLUGIN.resolve_title_from_mmap(path, Path::new("/"), data),
            None
        );
        let first_prompt = JsonlSession::replay(data)
            .messages
            .iter()
            .find_map(|m| m["content"][0]["text"].as_str().map(str::to_string));
        assert_eq!(
            first_prompt.as_deref(),
            Some("corrected prompt\nsecond line")
        );
        let clean = br#"{"sessionId":"s","projectHash":"h"}
{"id":"u1","type":"user","content":[{"text":"first line\nsecond line"}]}
"#;
        assert_eq!(
            PLUGIN
                .resolve_title_from_mmap(path, Path::new("/"), clean)
                .as_deref(),
            Some("first line")
        );
    }

    #[test]
    fn project_dir_named_tmp_is_not_confused_with_chats() {
        let home = Path::new("/home/u");
        let path =
            Path::new("/home/u/.gemini/tmp/tmp/chats/session-2026-06-01T00-00-abcd1234.jsonl");
        assert_eq!(PLUGIN.resolve_project(path, home).as_deref(), Some("tmp"));
        let logs = Path::new("/tmp/x/.gemini/tmp/proj/logs.json");
        assert_eq!(PLUGIN.resolve_project(logs, home).as_deref(), Some("proj"));
    }

    #[test]
    fn cwd_resolves_from_project_root_then_projects_json() {
        let base = std::env::temp_dir().join(format!("ah-gemini-cwd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("tmp/tmp/chats")).unwrap();
        fs::create_dir_all(base.join("tmp/myproj/chats")).unwrap();
        fs::write(base.join("tmp/tmp/.project_root"), "/tmp\n").unwrap();
        fs::write(
            base.join("projects.json"),
            r#"{"projects":{"/work/myproj":"myproj"}}"#,
        )
        .unwrap();
        let home = Path::new("/nonexistent");
        let name = "session-2026-06-01T00-00-abcd1234.jsonl";
        let tmp_session = base.join("tmp/tmp/chats").join(name);
        let proj_session = base.join("tmp/myproj/chats").join(name);
        let tmp_cwd = PLUGIN.resolve_cwd(&tmp_session, home);
        let proj_cwd = PLUGIN.resolve_cwd(&proj_session, home);
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(tmp_cwd.as_deref(), Some("/tmp"));
        assert_eq!(proj_cwd.as_deref(), Some("/work/myproj"));
    }

    #[test]
    fn file_name_time_is_utc() {
        let jst = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
        assert_eq!(
            file_time_in("2026-06-11T20-44", &jst).as_deref(),
            Some("2026-06-12 05:44")
        );
        assert_eq!(file_time_in("2026-13-01T00-00", &jst), None);

        let expected = file_time_in("2026-06-11T02-44", &Local).unwrap();
        for name in [
            "session-2026-06-11T02-44-acc93477.jsonl",
            "session-2026-06-11T02-44-1-acc93477.jsonl",
            "session-2026-06-11T02-44-acc93477.json",
        ] {
            let path = PathBuf::from("/h/.gemini/tmp/p/chats").join(name);
            assert_eq!(
                PLUGIN
                    .resolve_date(&path, SystemTime::UNIX_EPOCH)
                    .as_deref(),
                Some(expected.as_str()),
                "{name}"
            );
        }
    }
}

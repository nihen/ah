use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;
use std::time::SystemTime;

use chrono::{Local, NaiveDateTime, TimeZone, Utc};
use regex::Regex;
use sha2::{Digest, Sha256};

use super::AgentPlugin;
use super::Message;
use super::common::first_text_part;
use super::common::format_mtime;
use super::common::mmap_file;

/// Project directory of a session file: `tmp/{project}/chats/session-*` or
/// `tmp/{project}/logs.json`. Anchored to the end of the path so that a
/// project named `tmp` (cwd `/tmp`) does not capture `chats`.
static RE_GEMINI_TMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/tmp/([^/]+)/(?:chats/[^/]+|logs\.json)$").unwrap());
/// Session start time in the file name, written in UTC by Gemini CLI
/// (`new Date().toISOString()`).
static RE_GEMINI_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"session-(\d{4}-\d{2}-\d{2}T\d{2}-\d{2})(?:-[^/]*)?\.jsonl?$").unwrap()
});

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

/// Messages and session id reconstructed from a `session-*.jsonl` file.
///
/// The file is an append-only log: the first line holds session metadata,
/// message records are upserted by `id`, `{"$set": {...}}` updates metadata
/// (a `messages` array replaces the whole conversation), and
/// `{"$rewindTo": "<id>"}` drops that message and everything after it.
#[derive(Default)]
struct JsonlSession {
    session_id: Option<String>,
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
                if let Some(id) = set.get("sessionId").and_then(|v| v.as_str()) {
                    session.session_id = Some(id.to_string());
                }
                if let Some(serde_json::Value::Array(messages)) = set.remove("messages") {
                    session.clear();
                    messages.into_iter().for_each(|m| session.upsert(m));
                }
            } else if record.get("projectHash").is_some_and(|v| v.is_string()) {
                if let Some(id) = record.get("sessionId").and_then(|v| v.as_str()) {
                    session.session_id = Some(id.to_string());
                }
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

/// Session id of a `session-*.jsonl` file without replaying its messages:
/// the metadata line, overridden by any `$set.sessionId`.
fn jsonl_session_id(data: &[u8]) -> Option<String> {
    let mut session_id = None;
    for (n, line) in data.split(|&b| b == b'\n').enumerate() {
        let is_set = line.starts_with(b"{\"$set\"");
        if n > 0 && !is_set {
            continue;
        }
        if is_set && memchr::memmem::find(line, b"\"sessionId\"").is_none() {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let meta = if is_set {
            record.get("$set")
        } else {
            Some(&record)
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
            // Tool-call-only turns carry an empty "content"
            if let Some(text) = val
                .get("content")
                .and_then(first_text_part)
                .filter(|t| !t.is_empty())
            {
                return visit(Message::assistant(text.to_string()));
            }
        }
        _ => {}
    }
    true
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

    fn resolve_project(&self, path: &Path, _home: &Path) -> Option<String> {
        RE_GEMINI_TMP
            .captures(&path.to_string_lossy())
            .map(|caps| caps[1].to_string())
            .or_else(|| Some("?".to_string()))
    }

    fn resolve_date(&self, path: &Path, mtime: SystemTime) -> Option<String> {
        RE_GEMINI_DATE
            .captures(&path.to_string_lossy())
            .and_then(|caps| NaiveDateTime::parse_from_str(&caps[1], "%Y-%m-%dT%H-%M").ok())
            .map(|utc| {
                Utc.from_utc_datetime(&utc)
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .or_else(|| Some(format_mtime(mtime)))
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

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
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
    use super::super::MessageRole;
    use super::*;
    use std::path::PathBuf;

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
            ]
        );
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
        let utc = NaiveDateTime::parse_from_str("2026-06-11T02-44", "%Y-%m-%dT%H-%M").unwrap();
        let expected = Local
            .from_utc_datetime(&utc)
            .format("%Y-%m-%d %H:%M")
            .to_string();
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

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::format_mtime;
use super::common::mmap_file;
use super::common::strip_home;

static RE_CODEX_SESSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".*/sessions/(\d+/\d+/\d+)/.*").unwrap());
static RE_CODEX_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"rollout-(\d{4}-\d{2}-\d{2})T(\d{2})-(\d{2})").unwrap());
static RE_CODEX_ROLLOUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^rollout-[\dT-]+-(.+)$").unwrap());

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
    if let Some((cached_stamp, index)) = cache.get(index_path) {
        if *cached_stamp == stamp {
            return index.clone();
        }
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
        if let (Some(id), Some(name)) = (id, name) {
            if !name.is_empty() {
                names.insert(id.to_string(), name.to_string());
            }
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
    use memchr::memmem::Finder;
    static RESPONSE_ITEM: LazyLock<Finder<'static>> =
        LazyLock::new(|| Finder::new(b"\"response_item\""));
    static USER: LazyLock<Finder<'static>> = LazyLock::new(|| Finder::new(b"\"user\""));
    static ASSISTANT: LazyLock<Finder<'static>> = LazyLock::new(|| Finder::new(b"\"assistant\""));

    for line in data.split(|&b| b == b'\n') {
        if RESPONSE_ITEM.find(line).is_none()
            || (USER.find(line).is_none() && ASSISTANT.find(line).is_none())
        {
            continue;
        }
        let Ok(val) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if !visit_message_value(&val, visit) {
            return;
        }
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
                if is_user_text {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        if !text.starts_with('<')
                            && !text.starts_with("# ")
                            && !visit(Message::user(text.to_string()))
                        {
                            return false;
                        }
                    }
                }
            }
        }
        Some("assistant") => {
            for item in contents {
                if item.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        if !visit(Message::assistant(text.to_string())) {
                            return false;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    true
}

/// The `session_meta` fields ah reads from the first line of a rollout.
#[derive(Clone, serde::Deserialize)]
struct SessionMeta {
    id: Option<String>,
    cwd: Option<String>,
}

#[derive(serde::Deserialize)]
struct SessionMetaLine {
    payload: Option<SessionMeta>,
}

type CachedMeta = (PathBuf, Option<SystemTime>, Option<SessionMeta>);

thread_local! {
    static LAST_META: std::cell::RefCell<Option<CachedMeta>> =
        const { std::cell::RefCell::new(None) };
}

/// Session metadata from the first line, which Codex writes once. Listing a
/// session resolves its id, cwd and title separately, so each thread keeps
/// the last parsed line (keyed by path and mtime).
fn session_meta(path: &Path) -> Option<SessionMeta> {
    let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
    let cached = LAST_META.with_borrow(|last| {
        last.as_ref()
            .filter(|(p, t, _)| p == path && *t == mtime)
            .map(|(_, _, meta)| meta.clone())
    });
    if let Some(meta) = cached {
        return meta;
    }
    let meta = read_session_meta(path);
    LAST_META.set(Some((path.to_path_buf(), mtime, meta.clone())));
    meta
}

fn read_session_meta(path: &Path) -> Option<SessionMeta> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    serde_json::from_str::<SessionMetaLine>(&line).ok()?.payload
}

pub static PLUGIN: CodexPlugin = CodexPlugin;

pub struct CodexPlugin;

impl AgentPlugin for CodexPlugin {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn description(&self) -> &'static str {
        "Codex CLI (OpenAI)"
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative path of cwd)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".codex/sessions/**/*.jsonl",
            ".codex/archived_sessions/**/*.jsonl",
        ]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.codex/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        if let Some(mmap) = mmap_file(path) {
            for_each_message(&mmap, visit);
        }
    }

    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        for_each_message(data, visit);
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
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

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        let session_id = session_meta(path)?.id?;
        latest_thread_name(path, &session_id)
    }

    fn is_archived(&self, path: &Path) -> bool {
        sessions_root(path).and_then(|dir| dir.file_name()) == Some("archived_sessions".as_ref())
    }

    // `codex resume <id>` also finds archived sessions, so they keep their id.
    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        if let Some(id) = session_meta(path)
            .and_then(|m| m.id)
            .filter(|id| !id.is_empty())
        {
            return Some(id);
        }

        let stem = path.file_stem()?.to_string_lossy();
        RE_CODEX_ROLLOUT
            .captures(&stem)
            .map(|caps| caps[1].to_string())
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["codex".to_string(), "resume".to_string(), id])
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
}

use super::entries::{RecordVisitor, SearchKind as K, TypedVisitor};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};

use super::AgentPlugin;
use super::Message;
use super::common::canonicalize_if_exists;
use super::common::for_each_jsonl_value_bytes;
use super::common::is_safe_cli_id;
use super::common::mmap_file;
use super::common::percent_decode;
use super::common::strip_home;
use super::common::tagged_user_body;
use super::common::visit_tool_call;
use super::{MemoryKind, MemorySource};

pub static PLUGIN: AgyPlugin = AgyPlugin;

/// Google Antigravity CLI (`agy`).
///
/// Layout: `~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/transcript.jsonl`
/// The transcript itself carries no cwd, title, archive state, or parent id;
/// these are resolved from `~/.gemini/antigravity-cli/conversation_summaries.db`
/// and the cache files under `~/.gemini/antigravity-cli/cache/`
/// (`last_conversations.json`, `conversation_metadata.json`).
pub struct AgyPlugin;

/// Per-base metadata index built from Antigravity's SQLite summary database
/// and JSON cache files.
struct SessionIndex {
    cwd_by_id: HashMap<String, String>,
    title_by_id: HashMap<String, String>,
    parent_by_id: HashMap<String, String>,
    archived_ids: HashSet<String>,
}

fn pick_cwd_from_uris(uris: &[serde_json::Value]) -> Option<String> {
    let paths: Vec<String> = uris
        .iter()
        .filter_map(|u| u.as_str())
        .filter_map(|u| u.strip_prefix("file://"))
        .map(percent_decode)
        .filter(|p| !p.is_empty())
        .collect();
    // `agy --add-dir /tmp` appends /tmp as an extra workspace;
    // prefer the real project dir, fall back to whatever is first.
    paths
        .iter()
        .find(|p| p.as_str() != "/tmp")
        .or_else(|| paths.first())
        .map(|cwd| canonicalize_if_exists(cwd))
}

/// Open `db` read-only without creating `-wal`/`-shm` sidecars next to it.
/// Resolves symlinks first so a `-wal` file next to the real target is seen.
fn open_db_read_only(db: &Path) -> Option<Connection> {
    if !db.is_file() {
        return None;
    }
    let real = fs::canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let wal = format!("{}-wal", real.display());
    let conn = if Path::new(&wal).exists() {
        Connection::open_with_flags(&real, flags)
    } else {
        Connection::open_with_flags(
            format!("file:{}?immutable=1", super::opencode::uri_path(&real)),
            flags | OpenFlags::SQLITE_OPEN_URI,
        )
    }
    .ok()?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    Some(conn)
}

type FileStamp = Option<(SystemTime, u64)>;

fn file_stamp(path: &Path) -> FileStamp {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Snapshot of the backing metadata files' `(mtime, size)` so updates to the
/// SQLite DB, its WAL, or the JSON caches invalidate the cached `SessionIndex`.
fn index_stamp(base: &Path) -> [FileStamp; 4] {
    let db = base.join("conversation_summaries.db");
    let real_db = fs::canonicalize(&db).unwrap_or(db);
    let wal = PathBuf::from(format!("{}-wal", real_db.display()));
    [
        file_stamp(&real_db),
        file_stamp(&wal),
        file_stamp(&base.join("cache/conversation_metadata.json")),
        file_stamp(&base.join("cache/last_conversations.json")),
    ]
}

impl SessionIndex {
    fn load(base: &Path) -> Self {
        let mut cwd_by_id = HashMap::new();
        let mut title_by_id = HashMap::new();
        let mut parent_by_id = HashMap::new();
        let mut archived_ids = HashSet::new();

        // cache/conversation_metadata.json: {"conversations": {"<id>": {"summary": {"WorkspaceURIs": ["file:///path", ...]}}}}
        if let Ok(content) = fs::read_to_string(base.join("cache/conversation_metadata.json")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(convs) = val.get("conversations").and_then(|v| v.as_object()) {
                    for (id, entry) in convs {
                        let Some(uris) = entry
                            .pointer("/summary/WorkspaceURIs")
                            .and_then(|v| v.as_array())
                        else {
                            continue;
                        };
                        if let Some(cwd) = pick_cwd_from_uris(uris) {
                            cwd_by_id.insert(id.clone(), cwd);
                        }
                    }
                }
            }
        }

        // conversation_summaries.db: authoritative SQLite store kept up to date
        // by the Antigravity language server (cwd, title, parent, archive state).
        if let Some(conn) = open_db_read_only(&base.join("conversation_summaries.db")) {
            if let Ok(mut stmt) = conn.prepare(
                "SELECT conversation_id, title, workspace_uris, parent_conversation_id \
                 FROM conversation_summaries",
            ) {
                if let Ok(rows) = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1).unwrap_or_default(),
                        row.get::<_, String>(2).unwrap_or_default(),
                        row.get::<_, String>(3).unwrap_or_default(),
                    ))
                }) {
                    for (id, title, uris_json, parent) in rows.flatten() {
                        if let Ok(serde_json::Value::Array(uris)) =
                            serde_json::from_str::<serde_json::Value>(&uris_json)
                        {
                            if let Some(cwd) = pick_cwd_from_uris(&uris) {
                                cwd_by_id.insert(id.clone(), cwd);
                            }
                        }
                        let title = title.trim();
                        if !title.is_empty() {
                            title_by_id.insert(id.clone(), title.to_string());
                        }
                        let parent = parent.trim();
                        if !parent.is_empty() {
                            parent_by_id.insert(id, parent.to_string());
                        }
                    }
                }
            }

            if let Ok(mut stmt) =
                conn.prepare("SELECT conversation_id, parent_id FROM conversation_parents")
            {
                if let Ok(rows) = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                }) {
                    for (id, parent) in rows.flatten() {
                        let parent = parent.trim();
                        if !parent.is_empty() {
                            parent_by_id.entry(id).or_insert_with(|| parent.to_string());
                        }
                    }
                }
            }

            if let Ok(mut stmt) = conn.prepare(
                "SELECT DISTINCT conversation_id FROM conversations_fts \
                 WHERE CAST(archived AS INTEGER) != 0 OR lower(CAST(archived AS TEXT)) = 'true'",
            ) {
                if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                    for id in rows.flatten() {
                        archived_ids.insert(id);
                    }
                }
            }
        }

        // cache/last_conversations.json: {"<cwd>": "<id>"} — newer, overrides metadata.
        if let Ok(content) = fs::read_to_string(base.join("cache/last_conversations.json")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(map) = val.as_object() {
                    for (cwd, id) in map {
                        if let Some(id) = id.as_str() {
                            cwd_by_id.insert(id.to_string(), canonicalize_if_exists(cwd));
                        }
                    }
                }
            }
        }

        Self {
            cwd_by_id,
            title_by_id,
            parent_by_id,
            archived_ids,
        }
    }
}

struct CachedIndex {
    stamp: [FileStamp; 4],
    index: Arc<SessionIndex>,
}

static SESSION_INDEX_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedIndex>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn session_index(base: &Path) -> Arc<SessionIndex> {
    if let Ok(mut cache) = SESSION_INDEX_CACHE.lock() {
        let stamp = index_stamp(base);
        if let Some(entry) = cache.get(base) {
            if entry.stamp == stamp {
                return Arc::clone(&entry.index);
            }
        }
        // Snapshot the stamp and load while holding the lock so parallel cold
        // resolvers serialize and any write during load invalidates the cache
        // on the next lookup.
        let idx = Arc::new(SessionIndex::load(base));
        cache.insert(
            base.to_path_buf(),
            CachedIndex {
                stamp,
                index: Arc::clone(&idx),
            },
        );
        return idx;
    }
    Arc::new(SessionIndex::load(base))
}

impl AgyPlugin {
    /// Walk up from the transcript to find `brain/<id>`; returns (base, id).
    /// base is the directory containing `brain/` (i.e. `.../antigravity-cli`).
    fn split_path(path: &Path) -> Option<(PathBuf, String)> {
        let mut current = path.parent();
        while let Some(dir) = current {
            if let Some(parent) = dir.parent() {
                if parent.file_name().and_then(|s| s.to_str()) == Some("brain") {
                    let id = dir.file_name()?.to_string_lossy().to_string();
                    return Some((parent.parent()?.to_path_buf(), id));
                }
            }
            current = dir.parent();
        }
        None
    }

    fn message_from_value(val: &serde_json::Value) -> Option<Message> {
        let source = val.get("source").and_then(|v| v.as_str())?;
        let kind = val.get("type").and_then(|v| v.as_str())?;
        match (source, kind) {
            ("USER_EXPLICIT", "USER_INPUT") => {
                let raw = val.get("content").and_then(|v| v.as_str())?;
                let text = tagged_user_body(raw, "USER_REQUEST")?;
                Some(Message::user(text.to_string()).at(val.get("created_at")))
            }
            ("MODEL", "PLANNER_RESPONSE") => {
                let text = val.get("content").and_then(|v| v.as_str())?;
                if text.trim().is_empty() {
                    return None;
                }
                Some(Message::assistant(text.to_string()).at(val.get("created_at")))
            }
            _ => None,
        }
    }
}

impl AgentPlugin for AgyPlugin {
    fn clear_caches(&self) {
        if let Ok(mut c) = SESSION_INDEX_CACHE.lock() {
            c.clear();
        }
    }

    fn id(&self) -> &'static str {
        "agy"
    }

    fn description(&self) -> &'static str {
        "Antigravity CLI (Google)"
    }

    fn can_resume(&self) -> bool {
        true
    }
    fn can_detect_running(&self) -> bool {
        // Needs the kernel lock table (`/proc/locks`).
        cfg!(target_os = "linux")
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

    fn can_memory(&self) -> bool {
        true
    }

    /// Global customizations live in `~/.gemini/config/`.
    fn global_memory_sources(&self, home: &Path) -> Vec<MemorySource> {
        let config = home.join(".gemini/config");
        vec![
            MemorySource::new(&config, "rules/*.md", MemoryKind::Rule),
            MemorySource::new(&config, "skills/*/SKILL.md", MemoryKind::Skill),
        ]
    }

    fn project_memory_sources(&self, dir: &Path) -> Vec<MemorySource> {
        vec![
            MemorySource::new(dir, ".agents/rules/*.md", MemoryKind::Rule),
            MemorySource::new(dir, ".agent/rules/*.md", MemoryKind::Rule),
        ]
    }

    fn can_follow(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative cwd looked up by conversation id in antigravity-cli/conversation_summaries.db and cache/*.json)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[".gemini/antigravity-cli/brain/*/.system_generated/logs/transcript.jsonl"]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        // Longer than gemini's "/.gemini/" so it wins the longest-marker match.
        &["/.gemini/antigravity-cli/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        if let Some(mmap) = mmap_file(path) {
            self.iter_messages_from_bytes(path, &mmap, visit);
        }
    }

    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        for_each_jsonl_value_bytes(data, |val| match Self::message_from_value(val) {
            Some(msg) => visit(msg),
            None => true,
        });
    }

    fn messages_from_value(&self, val: &serde_json::Value) -> Vec<Message> {
        Self::message_from_value(val).into_iter().collect()
    }

    fn iter_search_records(&self, path: &Path, visit: &mut RecordVisitor<'_>) {
        if let Some(data) = self.session_bytes(path) {
            for_each_jsonl_value_bytes(&data, |val| {
                visit.record(|emit| visit_search_record(val, emit))
            });
        }
    }

    fn iter_search_texts(&self, path: &Path, visit: &mut dyn FnMut(&str) -> bool) {
        if let Some(mmap) = mmap_file(path) {
            self.iter_search_texts_from_bytes(path, &mmap, visit);
        }
    }

    /// Messages, the `tool_calls` of planner responses, and the content of
    /// other model steps (tool results such as file views and command
    /// output).
    fn iter_search_texts_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(&str) -> bool,
    ) {
        for_each_jsonl_value_bytes(data, |val| {
            visit_search_record(val, &mut |_, text| visit(text))
        });
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        let (base, id) = Self::split_path(path)?;
        session_index(&base).cwd_by_id.get(&id).cloned()
    }

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        let (base, id) = Self::split_path(path)?;
        session_index(&base).title_by_id.get(&id).cloned()
    }

    fn is_archived(&self, path: &Path) -> bool {
        let Some((base, id)) = Self::split_path(path) else {
            return false;
        };
        session_index(&base).archived_ids.contains(&id)
    }

    fn parent_session_id(&self, path: &Path) -> Option<String> {
        let (base, id) = Self::split_path(path)?;
        session_index(&base).parent_by_id.get(&id).cloned()
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        // Raw identifier: home-relative cwd (like codex). Basename is applied
        // by the resolver. Unknown cwd → "?" (same convention as gemini).
        self.resolve_cwd(path, home)
            .map(|cwd| strip_home(&cwd, home))
            .or_else(|| Some("?".to_string()))
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::split_path(path)
            .map(|(_, id)| id)
            .filter(|id| is_safe_cli_id(id))
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["agy".to_string(), "--conversation".to_string(), id])
    }

    fn running_sessions(&self) -> Vec<(String, Option<u32>)> {
        dirs::home_dir()
            .map(|home| {
                running_in(
                    &home.join(".gemini/antigravity-cli/presence"),
                    &super::common::file_lock_holders(),
                )
            })
            .unwrap_or_default()
    }
}

/// Running sessions from `presence/<conversation-id>.lock`. Antigravity holds
/// an exclusive `flock` on a conversation's lock file while a process has it
/// open; the files stay behind afterwards. Reading `/proc/locks` avoids
/// contending with `agy` for the lock.
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

fn visit_search_record(val: &serde_json::Value, visit: &mut TypedVisitor<'_>) -> bool {
    if let Some(message) = AgyPlugin::message_from_value(val) {
        if !visit(K::from_role(message.role), &message.text) {
            return false;
        }
    }
    if val.get("source").and_then(|v| v.as_str()) != Some("MODEL") {
        return true;
    }
    if val.get("type").and_then(|v| v.as_str()) == Some("PLANNER_RESPONSE") {
        return val
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .is_none_or(|calls| {
                calls.iter().all(|call| {
                    visit_tool_call(call.get("name"), call.get("args"), &mut |text| {
                        visit(K::ToolInput, text)
                    })
                })
            });
    }
    val.get("content")
        .and_then(|v| v.as_str())
        // MODEL/GENERIC records do not reliably identify a tool result.
        // Keep them searchable, but do not infer a kind from their text.
        .is_none_or(|text| visit(K::Unknown, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_transcript(base: &Path, id: &str, body: &str) -> PathBuf {
        let dir = base.join("brain").join(id).join(".system_generated/logs");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("transcript.jsonl");
        fs::File::create(&p)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        p
    }

    #[test]
    fn parses_user_request_and_planner_response() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(".gemini/antigravity-cli");
        let body = concat!(
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-08-20T08:11:44Z","content":"<USER_REQUEST>\nfix the bug\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: x\n</ADDITIONAL_METADATA>"}"#,
            "\n",
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"view_file","args":{}}]}"#,
            "\n",
            r#"{"step_index":2,"source":"MODEL","type":"VIEW_FILE","status":"DONE","content":"file body"}"#,
            "\n",
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Done.","thinking":"..."}"#,
            "\n",
            r#"{"step_index":4,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"sys"}"#,
            "\n",
        );
        let path = write_transcript(&base, "11111111-aaaa", body);
        let mut msgs = Vec::new();
        PLUGIN.iter_messages(&path, &mut |m| {
            msgs.push(m);
            true
        });
        assert_eq!(
            msgs,
            vec![
                // 2026-08-20T08:11:44Z, the step's `created_at`
                Message::user("fix the bug".to_string()).at(Some(&1787213504.into())),
                Message::assistant("Done.".to_string()),
            ]
        );
    }

    #[test]
    fn resolves_cwd_from_cache_files_and_resume_id_from_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(".gemini/antigravity-cli");
        fs::create_dir_all(base.join("cache")).unwrap();
        fs::write(
            base.join("cache/conversation_metadata.json"),
            r#"{"conversations":{
                "aaaa":{"summary":{"WorkspaceURIs":["file:///tmp","file:///data/home/me/old%20proj"]}},
                "bbbb":{"summary":{"WorkspaceURIs":["file:///data/home/me/stale"]}}
            }}"#,
        )
        .unwrap();
        fs::write(
            base.join("cache/last_conversations.json"),
            r#"{"/data/home/me/new-proj":"bbbb"}"#,
        )
        .unwrap();
        let home = tmp.path();

        let a = write_transcript(&base, "aaaa", "");
        let b = write_transcript(&base, "bbbb", "");
        let c = write_transcript(&base, "cccc", "");

        assert_eq!(
            PLUGIN.resolve_cwd(&a, home).as_deref(),
            Some("/data/home/me/old proj")
        );
        assert_eq!(
            PLUGIN.resolve_project(&a, home).as_deref(),
            Some("/data/home/me/old proj")
        );
        // last_conversations.json wins over conversation_metadata.json
        assert_eq!(
            PLUGIN.resolve_cwd(&b, home).as_deref(),
            Some("/data/home/me/new-proj")
        );
        assert_eq!(PLUGIN.resolve_cwd(&c, home), None);

        // cwd under home → home-relative raw project
        let d = write_transcript(&base, "dddd", "");
        fs::write(
            base.join("cache/last_conversations.json"),
            format!(
                r#"{{"/data/home/me/new-proj":"bbbb","{}":"dddd"}}"#,
                home.join("src/proj").to_string_lossy()
            ),
        )
        .unwrap();
        // index is cached per base dir; use a fresh base to pick up the new file
        let base2 = tmp.path().join("alt/.gemini/antigravity-cli");
        fs::create_dir_all(base2.join("cache")).unwrap();
        fs::copy(
            base.join("cache/last_conversations.json"),
            base2.join("cache/last_conversations.json"),
        )
        .unwrap();
        let _ = d;
        let d2 = write_transcript(&base2, "dddd", "");
        assert_eq!(
            PLUGIN.resolve_project(&d2, home).as_deref(),
            Some("src/proj")
        );
        assert_eq!(PLUGIN.resolve_project(&c, home).as_deref(), Some("?"));

        assert_eq!(PLUGIN.resolve_resume_id(&c, home).as_deref(), Some("cccc"));
        assert_eq!(
            PLUGIN.resume_args(&c, home),
            Some(vec![
                "agy".to_string(),
                "--conversation".to_string(),
                "cccc".to_string()
            ])
        );
    }

    #[test]
    fn resolves_metadata_from_sqlite_summary_db() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(".gemini/antigravity-cli");
        fs::create_dir_all(base.join("cache")).unwrap();

        // Stale cache entry; SQLite should win over conversation_metadata.json,
        // while last_conversations.json wins over SQLite.
        fs::write(
            base.join("cache/conversation_metadata.json"),
            r#"{"conversations":{
                "conv-a":{"summary":{"WorkspaceURIs":["file:///stale/path"]}},
                "conv-cache-only":{"summary":{"WorkspaceURIs":["file:///only/in/cache"]}}
            }}"#,
        )
        .unwrap();
        fs::write(
            base.join("cache/last_conversations.json"),
            r#"{"/newest/from/last":"conv-last"}"#,
        )
        .unwrap();

        let db = base.join("conversation_summaries.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                workspace_uris TEXT NOT NULL DEFAULT '',
                parent_conversation_id TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE conversation_parents (
                conversation_id TEXT PRIMARY KEY,
                parent_id TEXT NOT NULL
            );
            CREATE TABLE conversations_fts (
                conversation_id TEXT,
                archived INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            (
                "conv-a",
                "  Auth Refactor  ",
                r#"["file:///tmp","file:///data/home/me/my%20repo"]"#,
                "",
            ),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            (
                "conv-child",
                "Subtask",
                r#"["file:///data/home/me/my%20repo"]"#,
                "conv-a",
            ),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            ("conv-child2", "", r#"[]"#, ""),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_parents VALUES (?1, ?2)",
            ("conv-child2", "conv-a"),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            ("conv-last", "Last Conv", r#"["file:///from/db"]"#, ""),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations_fts VALUES (?1, ?2)",
            ("conv-a", 0),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations_fts VALUES (?1, ?2)",
            ("conv-last", 1),
        )
        .unwrap();
        drop(conn);

        let home = tmp.path();
        let a = write_transcript(&base, "conv-a", "");
        let child = write_transcript(&base, "conv-child", "");
        let child2 = write_transcript(&base, "conv-child2", "");
        let last = write_transcript(&base, "conv-last", "");
        let cache_only = write_transcript(&base, "conv-cache-only", "");

        assert_eq!(
            PLUGIN.resolve_cwd(&a, home).as_deref(),
            Some("/data/home/me/my repo")
        );
        assert_eq!(
            PLUGIN.resolve_title(&a, home).as_deref(),
            Some("Auth Refactor")
        );
        assert!(!PLUGIN.is_archived(&a));
        assert_eq!(PLUGIN.parent_session_id(&a), None);

        assert_eq!(PLUGIN.parent_session_id(&child).as_deref(), Some("conv-a"));
        assert!(PLUGIN.is_subagent(&child));
        assert_eq!(PLUGIN.parent_session_id(&child2).as_deref(), Some("conv-a"));
        assert_eq!(PLUGIN.resolve_title(&child2, home), None);

        assert_eq!(
            PLUGIN.resolve_cwd(&last, home).as_deref(),
            Some("/newest/from/last")
        );
        assert!(PLUGIN.is_archived(&last));

        assert_eq!(
            PLUGIN.resolve_cwd(&cache_only, home).as_deref(),
            Some("/only/in/cache")
        );
    }

    #[cfg(unix)]
    #[test]
    fn running_in_matches_held_presence_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let presence = tmp.path().join("presence");
        fs::create_dir_all(&presence).unwrap();

        let active = presence.join("conv-live.lock");
        let stale = presence.join("conv-stale.lock");
        let hidden = presence.join(".hidden.lock");
        let other = presence.join("readme.txt");
        fs::write(&active, "").unwrap();
        fs::write(&stale, "").unwrap();
        fs::write(&hidden, "").unwrap();
        fs::write(&other, "").unwrap();

        let mut holders = HashMap::new();
        holders.insert(super::super::common::file_lock_key(&active).unwrap(), 4242);
        holders.insert(super::super::common::file_lock_key(&hidden).unwrap(), 9999);

        let running = running_in(&presence, &holders);
        assert_eq!(running, vec![("conv-live".to_string(), Some(4242))]);
    }

    #[cfg(unix)]
    #[test]
    fn reads_wal_updates_through_symlink_and_invalidates_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        let base = tmp.path().join(".gemini/antigravity-cli");
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&base).unwrap();

        let real_db = real_dir.join("conversation_summaries.db");
        let conn = Connection::open(&real_db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                workspace_uris TEXT NOT NULL DEFAULT '',
                parent_conversation_id TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE conversations_fts (
                conversation_id TEXT,
                archived
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries VALUES ('c1', 'Before', '[\"file:///before\"]', '')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO conversations_fts VALUES ('c1', 'false')", [])
            .unwrap();
        // Force a checkpoint so the initial DB is clean, then keep WAL active.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();

        std::os::unix::fs::symlink(&real_db, base.join("conversation_summaries.db")).unwrap();

        let home = tmp.path();
        let p = write_transcript(&base, "c1", "");
        assert_eq!(PLUGIN.resolve_title(&p, home).as_deref(), Some("Before"));
        assert_eq!(PLUGIN.resolve_cwd(&p, home).as_deref(), Some("/before"));
        assert!(!PLUGIN.is_archived(&p));

        // Update in WAL mode without checkpointing: the changes live in real_db-wal.
        conn.execute(
            "UPDATE conversation_summaries SET title = 'After', workspace_uris = '[\"file:///after\"]', parent_conversation_id = 'p0' WHERE conversation_id = 'c1'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE conversations_fts SET archived = 'true' WHERE conversation_id = 'c1'",
            [],
        )
        .unwrap();

        assert_eq!(PLUGIN.resolve_title(&p, home).as_deref(), Some("After"));
        assert_eq!(PLUGIN.resolve_cwd(&p, home).as_deref(), Some("/after"));
        assert_eq!(PLUGIN.parent_session_id(&p).as_deref(), Some("p0"));
        assert!(PLUGIN.is_archived(&p));
        drop(conn);
    }
}

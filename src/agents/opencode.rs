use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::AgentPlugin;
use super::Message;
use super::common::SessionBytes;
use super::common::canonicalize_if_exists;
use super::common::is_safe_cli_id;
use super::common::strip_home;

pub static PLUGIN: OpencodePlugin = OpencodePlugin;

/// opencode (sst/opencode).
///
/// Sessions live in a SQLite database: `~/.local/share/opencode/opencode.db`
/// (`opencode-<channel>.db` for non-release channels). Each top-level session
/// is exposed as the virtual path `<db>/<session-id>`, which never exists on
/// disk. Child (subagent) sessions are skipped, like Claude's sidechains.
///
/// The searchable/raw form of a session is JSONL with one line per message:
/// `{"id":"msg_…","info":<message.data>,"parts":[<part.data>,…]}`.
pub struct OpencodePlugin;

/// Session metadata from the `session` table.
#[derive(Clone)]
struct SessionRow {
    directory: String,
    title: String,
    time_created: i64,
    time_updated: i64,
}

static SESSION_CACHE: LazyLock<Mutex<HashMap<PathBuf, Option<SessionRow>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Placeholder title opencode assigns until it generates a real one.
static RE_DEFAULT_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(New session|Child session) - \d{4}-\d{2}-\d{2}T[\d:.]+Z$").unwrap()
});

/// A cached read-only connection. Immutable connections remember the
/// database file's state so they can be reopened if it changes.
struct CachedConnection {
    conn: Connection,
    immutable_stamp: Option<(SystemTime, u64)>,
}

thread_local! {
    static CONNECTIONS: RefCell<HashMap<PathBuf, CachedConnection>> =
        RefCell::new(HashMap::new());
}

/// Databases already reported on stderr (one warning per database).
static WARNED: LazyLock<Mutex<std::collections::HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

fn warn_once(db: &Path, err: &str) {
    if let Ok(mut warned) = WARNED.lock() {
        if warned.insert(db.to_path_buf()) {
            // A failing stderr must not abort the listing.
            let _ = writeln!(
                std::io::stderr(),
                "ah: opencode: cannot read {}: {}",
                db.display(),
                err
            );
        }
    }
}

/// 9999-12-31T23:59:59Z; larger values would overflow date formatting.
const MAX_MILLIS: i64 = 253_402_300_799_000;

fn millis_to_system_time(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.clamp(0, MAX_MILLIS) as u64)
}

/// Split a virtual session path into (database path, session id).
fn split_virtual_path(path: &Path) -> Option<(&Path, &str)> {
    let id = path.file_name()?.to_str()?;
    let db = path.parent()?;
    if !id.starts_with("ses_") {
        return None;
    }
    Some((db, id))
}

/// Percent-encode a filesystem path for a SQLite `file:` URI.
fn uri_path(path: &Path) -> String {
    let mut out = String::new();
    for &b in path.to_string_lossy().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// State of an immutable database: its mtime and size, or `None` once a
/// `-wal` file exists (a writer is active and the file may change).
fn immutable_stamp(db: &Path) -> Option<(SystemTime, u64)> {
    // Resolve symlinks: SQLite keeps `-wal` next to the real file.
    let real = std::fs::canonicalize(db).ok()?;
    let mut wal = real.as_os_str().to_owned();
    wal.push("-wal");
    if Path::new(&wal).exists() {
        return None;
    }
    let meta = std::fs::metadata(&real).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Open `db` without writing anything next to it. A WAL database whose
/// `-wal` file is absent is fully checkpointed, so it is opened as
/// immutable; opening it normally would create `-wal`/`-shm` files (and
/// fails in a read-only directory).
fn open_read_only(db: &Path) -> Result<CachedConnection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let stamp = immutable_stamp(db);
    let conn = match stamp {
        Some(_) => Connection::open_with_flags(
            format!("file:{}?immutable=1", uri_path(db)),
            flags | OpenFlags::SQLITE_OPEN_URI,
        ),
        None => Connection::open_with_flags(db, flags),
    }
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    Ok(CachedConnection {
        conn,
        immutable_stamp: stamp,
    })
}

/// Run `f` with a read-only connection to `db`, cached per thread. An
/// immutable connection is reopened when the file changed since it was
/// opened (e.g. opencode started writing, or a picker stayed open).
fn with_connection<T>(
    db: &Path,
    f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> Result<T, String> {
    CONNECTIONS.with(|conns| {
        let mut conns = conns.borrow_mut();
        let stale = conns.get(db).is_some_and(|c| {
            c.immutable_stamp.is_some() && c.immutable_stamp != immutable_stamp(db)
        });
        if stale {
            conns.remove(db);
        }
        if !conns.contains_key(db) {
            if !db.is_file() {
                return Err("no such file".to_string());
            }
            conns.insert(db.to_path_buf(), open_read_only(db)?);
        }
        f(&conns[db].conn).map_err(|e| e.to_string())
    })
}

/// A TEXT or BLOB column as bytes (NULL and numbers as empty).
fn column_bytes(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Vec<u8>> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Text(b) | ValueRef::Blob(b) => b.to_vec(),
        _ => Vec::new(),
    })
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        directory: row.get("directory")?,
        title: row.get("title")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
    })
}

/// Session metadata for a virtual path (cached; queried on a cache miss,
/// e.g. `ah show <path>` without a prior collection).
fn session_row(path: &Path) -> Option<SessionRow> {
    if let Ok(cache) = SESSION_CACHE.lock() {
        if let Some(row) = cache.get(path) {
            return row.clone();
        }
    }
    let (db, id) = split_virtual_path(path)?;
    let row = match with_connection(db, |conn| {
        conn.query_row(
            "SELECT directory, title, time_created, time_updated \
             FROM session WHERE id = ?1 AND parent_id IS NULL",
            [id],
            row_to_session,
        )
        .optional()
    }) {
        Ok(row) => row,
        Err(e) => {
            // Not cached: a read failure is not "no such session".
            warn_once(db, &e);
            return None;
        }
    };
    if let Ok(mut cache) = SESSION_CACHE.lock() {
        cache.insert(path.to_path_buf(), row.clone());
    }
    row
}

/// Build the JSONL form of a session (see `OpencodePlugin`). Both queries
/// run in one read transaction so they see the same snapshot.
fn export_session(db: &Path, id: &str) -> Result<Vec<u8>, String> {
    with_connection(db, |conn| {
        let tx = conn.unchecked_transaction()?;
        let mut parts: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT message_id, data FROM part WHERE session_id = ?1 ORDER BY message_id, id",
            )?;
            let mut rows = stmt.query([id])?;
            while let Some(row) = rows.next()? {
                parts
                    .entry(row.get(0)?)
                    .or_default()
                    .push(column_bytes(row, 1)?);
            }
        }

        let mut out = Vec::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id",
            )?;
            let mut rows = stmt.query([id])?;
            while let Some(row) = rows.next()? {
                let message_id: String = row.get(0)?;
                let id_json = serde_json::Value::String(message_id.clone()).to_string();
                out.extend_from_slice(b"{\"id\":");
                out.extend_from_slice(id_json.as_bytes());
                out.extend_from_slice(b",\"info\":");
                out.extend_from_slice(&column_bytes(row, 1)?);
                out.extend_from_slice(b",\"parts\":[");
                if let Some(message_parts) = parts.get(&message_id) {
                    for (i, part) in message_parts.iter().enumerate() {
                        if i > 0 {
                            out.push(b',');
                        }
                        out.extend_from_slice(part);
                    }
                }
                out.extend_from_slice(b"]}\n");
            }
        }
        tx.finish()?;
        Ok(out)
    })
}

/// Byte length of `export_session` output, computed in SQL. Each line adds
/// 29 bytes of framing (`{"id":""`, `,"info":`, `,"parts":[`, `]}\n`) plus
/// one comma between parts. Message ids are ASCII, so need no escaping.
fn export_size(db: &Path, id: &str) -> Result<u64, String> {
    with_connection(db, |conn| {
        conn.query_row(
            "SELECT \
               (SELECT coalesce(sum(29 + length(CAST(id AS BLOB)) + length(CAST(data AS BLOB))), 0) \
                  FROM message WHERE session_id = ?1) \
             + (SELECT coalesce(sum(length(CAST(p.data AS BLOB)) + 1), 0) - count(DISTINCT p.message_id) \
                  FROM part p JOIN message m ON m.id = p.message_id \
                  WHERE p.session_id = ?1 AND m.session_id = ?1)",
            [id],
            |row| row.get::<_, i64>(0),
        )
        .map(|size| size.max(0) as u64)
    })
}

/// Messages of a session, reading only text parts (tool output can be
/// orders of magnitude larger and is only needed for full-text search).
/// opencode serializes compact JSON with `type` first, so a prefix match
/// selects text parts without parsing large tool parts; any other layout
/// (e.g. small step markers) falls back to JSON inspection.
fn text_messages(db: &Path, id: &str) -> Result<Vec<Message>, String> {
    with_connection(db, |conn| {
        let tx = conn.unchecked_transaction()?;
        // One pass over the session's parts (a per-message join would rescan
        // them for every message via the session index).
        let mut parts: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT message_id, data FROM part WHERE session_id = ?1 \
                   AND (data LIKE '{\"type\":\"text\"%' \
                     OR (data NOT LIKE '{\"type\":\"%' \
                       AND json_valid(data) AND json_extract(data, '$.type') = 'text')) \
                 ORDER BY message_id, id",
            )?;
            let mut rows = stmt.query([id])?;
            while let Some(row) = rows.next()? {
                if let Ok(part) = serde_json::from_slice(&column_bytes(row, 1)?) {
                    parts.entry(row.get(0)?).or_default().push(part);
                }
            }
        }
        // Messages are collected before visiting so the read transaction is
        // not held while a slow consumer (e.g. a pager) blocks output.
        let mut messages = Vec::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id",
            )?;
            let mut rows = stmt.query([id])?;
            while let Some(row) = rows.next()? {
                let message_id: String = row.get(0)?;
                let Some(message_parts) = parts.get(&message_id) else {
                    continue;
                };
                let info: serde_json::Value =
                    serde_json::from_slice(&column_bytes(row, 1)?).unwrap_or_default();
                messages.extend(message_from_parts(&info, message_parts));
            }
        }
        tx.finish()?;
        Ok(messages)
    })
}

/// Text of a message: its non-synthetic, non-ignored text parts joined.
/// Compaction summaries (`summary: true`) are not part of the conversation.
fn message_from_parts(info: &serde_json::Value, parts: &[serde_json::Value]) -> Option<Message> {
    let role = info.get("role").and_then(|v| v.as_str())?;
    if info.get("summary").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    let texts: Vec<&str> = parts
        .iter()
        .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter(|p| {
            !p.get("synthetic")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .filter(|p| !p.get("ignored").and_then(|v| v.as_bool()).unwrap_or(false))
        .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if texts.is_empty() {
        return None;
    }
    let text = texts.join("\n\n");
    match role {
        "user" => Some(Message::user(text)),
        "assistant" => Some(Message::assistant(text)),
        _ => None,
    }
}

impl AgentPlugin for OpencodePlugin {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn description(&self) -> &'static str {
        "opencode"
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative session directory from opencode.db)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[".local/share/opencode/opencode*.db"]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/opencode/opencode"]
    }

    fn expand_sessions(&self, file: &Path) -> Option<Vec<(PathBuf, SystemTime)>> {
        let rows = with_connection(file, |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, directory, title, time_created, time_updated \
                 FROM session WHERE parent_id IS NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>("id")?, row_to_session(row)?))
            })?;
            // Skip malformed rows instead of dropping every session.
            let mut ok = Vec::new();
            for row in rows {
                match row {
                    Ok(row) => ok.push(row),
                    Err(e) => warn_once(file, &e.to_string()),
                }
            }
            Ok(ok)
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => {
                warn_once(file, &e);
                Vec::new()
            }
        };

        let mut sessions = Vec::with_capacity(rows.len());
        let mut cache = SESSION_CACHE.lock().ok();
        for (id, row) in rows {
            let path = file.join(&id);
            sessions.push((path.clone(), millis_to_system_time(row.time_updated)));
            if let Some(cache) = cache.as_mut() {
                cache.insert(path, Some(row));
            }
        }
        Some(sessions)
    }

    fn session_mtime(&self, path: &Path) -> Option<SystemTime> {
        session_row(path).map(|row| millis_to_system_time(row.time_updated))
    }

    fn session_created(&self, path: &Path) -> Option<SystemTime> {
        session_row(path).map(|row| millis_to_system_time(row.time_created))
    }

    fn session_size(&self, path: &Path) -> Option<u64> {
        let (db, id) = split_virtual_path(path)?;
        session_row(path)?;
        export_size(db, id).map_err(|e| warn_once(db, &e)).ok()
    }

    /// Empty sessions have no bytes, like empty session files (so a regex
    /// matching the empty string does not select them).
    fn session_bytes(&self, path: &Path) -> Option<SessionBytes> {
        let (db, id) = split_virtual_path(path)?;
        session_row(path)?;
        export_session(db, id)
            .map_err(|e| warn_once(db, &e))
            .ok()
            .filter(|bytes| !bytes.is_empty())
            .map(SessionBytes::Owned)
    }

    fn cheap_session_bytes(&self) -> bool {
        false
    }

    fn raw_content(&self, path: &Path) -> Option<String> {
        let (db, id) = split_virtual_path(path)?;
        session_row(path)?;
        let bytes = export_session(db, id).map_err(|e| warn_once(db, &e)).ok()?;
        String::from_utf8(bytes).ok()
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        let Some((db, id)) = split_virtual_path(path) else {
            return;
        };
        match text_messages(db, id) {
            Ok(messages) => {
                for msg in messages {
                    if !visit(msg) {
                        break;
                    }
                }
            }
            Err(e) => warn_once(db, &e),
        }
    }

    /// Preloaded bytes are the full export including tool output; the text
    /// query is much cheaper than re-parsing them.
    fn iter_messages_from_bytes(
        &self,
        path: &Path,
        _data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        self.iter_messages(path, visit);
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        session_row(path)
            .map(|row| row.directory)
            .filter(|dir| !dir.is_empty())
            .map(|dir| canonicalize_if_exists(&dir))
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        self.resolve_cwd(path, home)
            .map(|cwd| strip_home(&cwd, home))
    }

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        // Placeholder titles fall back to the first prompt in the resolver.
        session_row(path)
            .map(|row| row.title.trim().to_string())
            .filter(|title| !title.is_empty() && !RE_DEFAULT_TITLE.is_match(title))
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        split_virtual_path(path)
            .map(|(_, id)| id.to_string())
            .filter(|id| is_safe_cli_id(id))
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["opencode".to_string(), "--session".to_string(), id])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a minimal opencode database (WAL mode, like opencode itself).
    /// The writer connection is closed before returning, so the -wal/-shm
    /// files are gone when the plugin opens it read-only.
    fn create_db(dir: &Path) -> PathBuf {
        let db = dir.join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
                parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
                title TEXT NOT NULL, version TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL);
            INSERT INTO session VALUES
                ('ses_aaa', 'p', NULL, 'a', '/data/home/me/proj', 'My title', '1',
                 1700000000000, 1700000100000),
                ('ses_child', 'p', 'ses_aaa', 'c', '/data/home/me/proj', 'Subagent', '1',
                 1700000000000, 1700000200000),
                ('ses_empty', 'p', NULL, 'e', '/data/home/me/proj',
                 'New session - 2026-09-26T05:01:52.113Z', '1', 1700000000000, 1700000000000);
            INSERT INTO message VALUES
                ('msg_1', 'ses_aaa', 1, 1, '{"role":"user","time":{"created":1}}'),
                ('msg_2', 'ses_aaa', 2, 2, '{"role":"assistant","time":{"created":2}}'),
                ('msg_3', 'ses_aaa', 3, 3, '{"role":"assistant","time":{"created":3}}'),
                ('msg_4', 'ses_aaa', 4, 4, '{"role":"assistant","summary":true,"mode":"compaction"}'),
                ('msg_5', 'ses_aaa', 5, 5, '{"role":"user","summary":{"diffs":[]}}');
            INSERT INTO part VALUES
                ('prt_1', 'msg_1', 'ses_aaa', 1, 1, '{"type":"text","text":"hello there"}'),
                ('prt_2', 'msg_1', 'ses_aaa', 1, 1,
                 '{"type":"text","text":"Called the Read tool","synthetic":true}'),
                ('prt_3', 'msg_2', 'ses_aaa', 2, 2,
                 '{"type":"tool","tool":"bash","state":{"output":"needle-in-tool"}}'),
                ('prt_4', 'msg_3', 'ses_aaa', 3, 3, '{"type":"reasoning","text":"thinking"}'),
                ('prt_5', 'msg_3', 'ses_aaa', 3, 3, '{"type":"text","text":"hi back"}'),
                ('prt_6', 'msg_3', 'ses_aaa', 3, 3, '{"type":"text","text":"skip","ignored":true}'),
                ('prt_7', 'msg_4', 'ses_aaa', 4, 4, '{"type":"text","text":"compaction summary"}'),
                ('prt_8', 'msg_5', 'ses_aaa', 5, 5, '{"type":"text","text":"second prompt"}');
            "#,
        )
        .unwrap();
        drop(conn);
        db
    }

    fn collect_messages(path: &Path) -> Vec<Message> {
        let mut msgs = Vec::new();
        PLUGIN.iter_messages(path, &mut |m| {
            msgs.push(m);
            true
        });
        msgs
    }

    #[test]
    fn expands_top_level_sessions_only() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let mut sessions = PLUGIN.expand_sessions(&db).unwrap();
        sessions.sort();
        assert_eq!(
            sessions,
            vec![
                (db.join("ses_aaa"), millis_to_system_time(1700000100000)),
                (db.join("ses_empty"), millis_to_system_time(1700000000000)),
            ]
        );
    }

    #[test]
    fn opening_does_not_create_wal_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        PLUGIN.expand_sessions(&db).unwrap();
        assert!(!db.with_file_name("opencode.db-wal").exists());
        assert!(!db.with_file_name("opencode.db-shm").exists());
    }

    #[test]
    fn parses_user_and_assistant_text_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let expected = vec![
            Message::user("hello there".to_string()),
            Message::assistant("hi back".to_string()),
            Message::user("second prompt".to_string()),
        ];
        let path = db.join("ses_aaa");
        assert_eq!(collect_messages(&path), expected);

        // The full-text export parses to the same messages.
        let bytes = PLUGIN.session_bytes(&path).unwrap();
        let mut from_bytes = Vec::new();
        PLUGIN.iter_messages_from_bytes(&path, &bytes, &mut |m| {
            from_bytes.push(m);
            true
        });
        assert_eq!(from_bytes, expected);
    }

    #[test]
    fn iteration_stops_when_visitor_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let mut count = 0;
        PLUGIN.iter_messages(&db.join("ses_aaa"), &mut |_| {
            count += 1;
            false
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn session_bytes_include_tool_output_and_match_size() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let path = db.join("ses_aaa");
        let bytes = PLUGIN.session_bytes(&path).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("needle-in-tool"));
        assert_eq!(text.lines().count(), 5);
        assert_eq!(PLUGIN.raw_content(&path).as_deref(), Some(text));
        assert_eq!(PLUGIN.session_size(&path), Some(bytes.len() as u64));
    }

    #[test]
    fn empty_session_has_empty_content() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let path = db.join("ses_empty");
        assert_eq!(PLUGIN.raw_content(&path).as_deref(), Some(""));
        assert_eq!(PLUGIN.session_size(&path), Some(0));
        assert!(collect_messages(&path).is_empty());
    }

    #[test]
    fn resolves_metadata_from_session_row() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let path = db.join("ses_aaa");
        let home = tmp.path();
        assert_eq!(
            PLUGIN.resolve_cwd(&path, home).as_deref(),
            Some("/data/home/me/proj")
        );
        assert_eq!(
            PLUGIN.resolve_title(&path, home).as_deref(),
            Some("My title")
        );
        assert_eq!(
            PLUGIN.session_mtime(&path),
            Some(millis_to_system_time(1700000100000))
        );
        assert_eq!(
            PLUGIN.session_created(&path),
            Some(millis_to_system_time(1700000000000))
        );
        assert_eq!(
            PLUGIN.resume_args(&path, home),
            Some(vec![
                "opencode".to_string(),
                "--session".to_string(),
                "ses_aaa".to_string()
            ])
        );
    }

    #[test]
    fn placeholder_title_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        assert_eq!(
            PLUGIN.resolve_title(&db.join("ses_empty"), tmp.path()),
            None
        );
    }

    #[test]
    fn project_raw_is_home_relative_when_cwd_is_under_home() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let home = Path::new("/data/home/me");
        assert_eq!(
            PLUGIN.resolve_project(&db.join("ses_aaa"), home).as_deref(),
            Some("proj")
        );
    }

    #[test]
    fn missing_or_child_sessions_do_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        assert_eq!(PLUGIN.session_mtime(&db.join("ses_nope")), None);
        assert_eq!(PLUGIN.session_mtime(&db.join("ses_child")), None);
        assert_eq!(PLUGIN.session_mtime(&tmp.path().join("x.db/ses_aaa")), None);
        assert!(PLUGIN.session_bytes(&db.join("ses_nope")).is_none());
        assert_eq!(PLUGIN.session_size(&db.join("ses_nope")), None);
    }

    #[test]
    fn unreadable_database_expands_to_no_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        std::fs::write(&db, "not a database").unwrap();
        assert_eq!(PLUGIN.expand_sessions(&db), Some(Vec::new()));
    }

    #[test]
    fn out_of_range_timestamps_are_clamped() {
        assert_eq!(millis_to_system_time(-1), UNIX_EPOCH);
        assert_eq!(
            millis_to_system_time(i64::MAX),
            millis_to_system_time(MAX_MILLIS)
        );
        // Must not panic when formatted.
        super::super::common::format_mtime(millis_to_system_time(i64::MAX));
    }

    #[test]
    fn text_part_with_non_compact_json_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            INSERT INTO session VALUES ('ses_sp', 'p', NULL, 's', '/x', 'Spaced', '1', 1, 1);
            INSERT INTO message VALUES ('msg_s', 'ses_sp', 1, 1, '{"role": "user"}');
            INSERT INTO part VALUES ('prt_s', 'msg_s', 'ses_sp', 1, 1, '{"type": "text", "text": "spaced"}');
            "#,
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            collect_messages(&db.join("ses_sp")),
            vec![Message::user("spaced".to_string())]
        );
    }

    #[test]
    fn empty_session_has_no_search_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        assert!(PLUGIN.session_bytes(&db.join("ses_empty")).is_none());
    }

    #[test]
    fn immutable_connection_is_reopened_after_the_database_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 2);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = DELETE; \
             INSERT INTO session VALUES ('ses_new', 'p', NULL, 'n', '/x', 'New', '1', 1, 1);",
        )
        .unwrap();
        drop(conn);
        // The size may not change and mtime granularity is coarse on some
        // filesystems; make the change observable.
        std::fs::File::options()
            .write(true)
            .open(&db)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(10))
            .unwrap();
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 3);
    }

    #[test]
    fn active_writer_is_read_through_its_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 2);
        // A writer keeps its connection open: the new row lives in the -wal.
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch(
                "INSERT INTO session VALUES ('ses_wal', 'p', NULL, 'w', '/x', 'Wal', '1', 1, 1);",
            )
            .unwrap();
        assert!(db.with_file_name("opencode.db-wal").exists());
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 3);
        drop(writer);
    }

    #[test]
    fn malformed_session_rows_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "INSERT INTO session VALUES ('ses_bad', 'p', NULL, 'b', '/x', 'Bad', '1', 1, 'soon');",
        )
        .unwrap();
        drop(conn);
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 2);
    }

    #[test]
    fn uri_path_escapes_special_characters() {
        assert_eq!(
            uri_path(Path::new("/a b/c?d#e%f.db")),
            "/a%20b/c%3Fd%23e%25f.db"
        );
    }
}

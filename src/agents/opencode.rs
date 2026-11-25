use super::entries::{RecordVisitor, SearchKind as K, TypedVisitor};
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
use super::{MemoryKind, MemorySource};

pub static PLUGIN: OpencodePlugin = OpencodePlugin;

/// opencode's config directory: `$XDG_CONFIG_HOME/opencode` or
/// `~/.config/opencode`.
fn opencode_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("opencode")
}

/// Files listed in the `instructions` array of `opencode.json` /
/// `opencode.jsonc` in `dir`. Entries may be globs; relative ones resolve
/// against `dir`, `~/` against the home directory. URLs are skipped.
fn configured_instructions(dir: &Path, home: &Path) -> Vec<MemorySource> {
    let mut sources = Vec::new();
    for name in ["opencode.json", "opencode.jsonc"] {
        let Ok(content) = std::fs::read_to_string(dir.join(name)) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(&content)) else {
            continue;
        };
        let entries = config
            .get("instructions")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str());
        for entry in entries {
            if entry.is_empty() || entry.contains("://") {
                continue;
            }
            let source = if let Some(rest) = entry.strip_prefix("~/") {
                MemorySource::new(home, rest, MemoryKind::Instruction)
            } else if Path::new(entry).is_absolute() {
                MemorySource::new(Path::new(""), entry, MemoryKind::Instruction)
            } else {
                MemorySource::new(dir, entry, MemoryKind::Instruction)
            };
            sources.push(source);
        }
    }
    sources
}

/// Remove `//` and `/* */` comments and trailing commas from JSONC so it
/// parses as JSON. String contents are left untouched.
fn strip_jsonc(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    // Trailing commas: a comma followed only by whitespace before `}` / `]`.
    let mut cleaned = String::with_capacity(out.len());
    let bytes: Vec<char> = out.chars().collect();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            cleaned.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                cleaned.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            cleaned.push(c);
        } else if c == ',' {
            let next = bytes[i + 1..].iter().find(|n| !n.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                cleaned.push(c);
            }
        } else {
            cleaned.push(c);
        }
        i += 1;
    }
    cleaned
}

/// opencode (sst/opencode).
///
/// Sessions live in a SQLite database: `~/.local/share/opencode/opencode.db`
/// (`opencode-<channel>.db` for non-release channels). Each top-level session
/// is exposed as the virtual path `<db>/<session-id>`, which never exists on
/// disk. Child (subagent) sessions are listed only with `--subagents`.
///
/// The searchable/raw form of a session is JSONL with one line per message.
/// It follows the tables the database keeps its sessions in (`Schema`):
/// `{"id":"msg_…","info":<message.data>,"parts":[<part.data>,…]}` for
/// opencode 1.x, `{"id":"msg_…","type":"user","data":<session_message.data>}`
/// for opencode 2.
pub struct OpencodePlugin;

/// The tables a database keeps its sessions in.
#[derive(Clone, Copy, PartialEq)]
enum Schema {
    /// opencode 1.x: `session`, `message` and `part`.
    V1,
    /// opencode 2: `session_v2` and `session_message`. Upgrading copies
    /// every 1.x session and its messages here, and opencode writes nowhere
    /// else afterwards, so the 1.x tables are not read.
    V2,
}

impl Schema {
    fn of(conn: &Connection) -> rusqlite::Result<Self> {
        let tables: i64 = conn
            .prepare_cached(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type = 'table' AND name IN ('session_v2', 'session_message')",
            )?
            .query_row([], |row| row.get(0))?;
        Ok(if tables == 2 { Self::V2 } else { Self::V1 })
    }

    fn session_table(self) -> &'static str {
        match self {
            Self::V1 => "session",
            Self::V2 => "session_v2",
        }
    }
}

/// Session metadata from the session table.
#[derive(Clone)]
struct SessionRow {
    parent_id: Option<String>,
    directory: String,
    title: String,
    time_created: i64,
    time_updated: i64,
    archived: bool,
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
pub(super) fn uri_path(path: &Path) -> String {
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

fn time_archived_expr(conn: &Connection, table: &str) -> rusqlite::Result<&'static str> {
    let sql = match table {
        "session_v2" => {
            "SELECT 1 FROM pragma_table_info('session_v2') WHERE name = 'time_archived' LIMIT 1"
        }
        _ => "SELECT 1 FROM pragma_table_info('session') WHERE name = 'time_archived' LIMIT 1",
    };
    let has_col = conn
        .prepare_cached(sql)?
        .query_row([], |_| Ok(()))
        .optional()?
        .is_some();
    Ok(if has_col {
        "time_archived"
    } else {
        "NULL AS time_archived"
    })
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let time_archived: Option<i64> = row.get("time_archived")?;
    Ok(SessionRow {
        parent_id: row.get("parent_id")?,
        directory: row.get("directory")?,
        // opencode 2 leaves the title NULL until it has generated one.
        title: row.get::<_, Option<String>>("title")?.unwrap_or_default(),
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
        archived: time_archived.is_some_and(|t| t != 0),
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
        let table = Schema::of(conn)?.session_table();
        let archived_col = time_archived_expr(conn, table)?;
        conn.prepare_cached(&format!(
            "SELECT parent_id, directory, title, time_created, time_updated, {archived_col} \
             FROM {table} WHERE id = ?1"
        ))?
        .query_row([id], row_to_session)
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
        if Schema::of(&tx)? == Schema::V2 {
            let mut out = Vec::new();
            {
                let mut stmt = tx.prepare_cached(
                    "SELECT id, type, data FROM session_message \
                     WHERE session_id = ?1 ORDER BY seq, id",
                )?;
                let mut rows = stmt.query([id])?;
                while let Some(row) = rows.next()? {
                    for (key, idx) in [("{\"id\":", 0), (",\"type\":", 1)] {
                        let text = serde_json::Value::String(row.get(idx)?).to_string();
                        out.extend_from_slice(key.as_bytes());
                        out.extend_from_slice(text.as_bytes());
                    }
                    out.extend_from_slice(b",\"data\":");
                    out.extend_from_slice(&column_bytes(row, 2)?);
                    out.extend_from_slice(b"}\n");
                }
            }
            tx.finish()?;
            return Ok(out);
        }
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

/// Byte length of `export_session` output, computed in SQL. Each 1.x line
/// adds 29 bytes of framing (`{"id":""`, `,"info":`, `,"parts":[`, `]}\n`)
/// plus one comma between parts; each opencode 2 line adds 28 (`{"id":""`,
/// `,"type":""`, `,"data":`, `}\n`). Message ids and types are ASCII, so
/// need no escaping.
fn export_size(db: &Path, id: &str) -> Result<u64, String> {
    with_connection(db, |conn| {
        if Schema::of(conn)? == Schema::V2 {
            return conn
                .prepare_cached(
                    "SELECT coalesce(sum(28 + length(CAST(id AS BLOB)) \
                       + length(CAST(type AS BLOB)) + length(CAST(data AS BLOB))), 0) \
                     FROM session_message WHERE session_id = ?1",
                )?
                .query_row([id], |row| row.get::<_, i64>(0))
                .map(|size| size.max(0) as u64);
        }
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
        if Schema::of(&tx)? == Schema::V2 {
            let messages = text_messages_v2(&tx, id)?;
            tx.finish()?;
            return Ok(messages);
        }
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

/// Messages of an opencode 2 session: the `text` of a `user` row and the
/// text items of an `assistant` row's `content`, picked in SQL so that tool
/// output, which is inline in the same JSON, is not copied out. Rows of any
/// other type (`system`, `synthetic`, `compaction`, `idle`, …) are not part
/// of the conversation.
///
/// A JSON function fails the whole query on text that is not JSON, and
/// `json_extract` returns objects and arrays as text too, so every value is
/// checked first, inside `CASE` (which evaluates only the branch it takes).
fn text_messages_v2(conn: &Connection, id: &str) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.type, json_extract(m.data, '$.time.created'), \
                CASE WHEN m.type = 'user' THEN \
                       CASE WHEN json_type(m.data, '$.text') = 'text' \
                            THEN json_extract(m.data, '$.text') END \
                     WHEN json_type(c.value, '$.text') = 'text' \
                       THEN json_extract(c.value, '$.text') END \
         FROM session_message m \
         LEFT JOIN json_each(CASE WHEN m.type = 'assistant' THEN \
                               CASE WHEN json_valid(m.data) THEN \
                                 CASE WHEN json_type(m.data, '$.content') = 'array' \
                                      THEN m.data END END END, '$.content') c \
           ON CASE WHEN c.type = 'object' THEN json_extract(c.value, '$.type') END = 'text' \
         WHERE m.session_id = ?1 AND m.type IN ('user', 'assistant') \
           AND CASE WHEN json_valid(m.data) THEN json_type(m.data) END = 'object' \
         ORDER BY m.seq, m.id, c.key",
    )?;
    let mut rows = stmt.query([id])?;
    // One row per text; the texts of a message are joined like 1.x parts.
    let mut messages: Vec<(String, Message)> = Vec::new();
    while let Some(row) = rows.next()? {
        let ValueRef::Text(text) = row.get_ref(3)? else {
            continue;
        };
        let text = String::from_utf8_lossy(text);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let message_id: String = row.get(0)?;
        if let Some((last_id, last)) = messages.last_mut() {
            if *last_id == message_id {
                last.text.push_str("\n\n");
                last.text.push_str(text);
                continue;
            }
        }
        let time = match row.get_ref(2)? {
            ValueRef::Integer(ms) => Some(ms.into()),
            ValueRef::Real(ms) => Some(ms.into()),
            _ => None,
        };
        let message = match row.get_ref(1)? {
            ValueRef::Text(b"user") => Message::user(text.to_string()),
            _ => Message::assistant(text.to_string()),
        };
        messages.push((message_id, message.at(time.as_ref())));
    }
    Ok(messages.into_iter().map(|(_, message)| message).collect())
}

/// Models of the session's assistant messages (`modelID`; `model.id` in
/// opencode 2), in order. Compaction summaries and responses interrupted
/// before any text, reasoning or tool part are not answers.
fn session_models(db: &Path, id: &str) -> Result<Option<super::common::ModelList>, String> {
    with_connection(db, |conn| {
        let mut stmt = conn.prepare_cached(match Schema::of(conn)? {
            Schema::V1 => {
                "SELECT json_extract(data, '$.modelID') FROM message \
                 WHERE session_id = ?1 AND json_valid(data) \
                   AND json_extract(data, '$.role') = 'assistant' \
                   AND json_type(data, '$.modelID') = 'text' \
                   AND coalesce(json_extract(data, '$.summary'), 0) != 1 \
                   AND id IN (SELECT part.message_id FROM part WHERE part.session_id = ?1 \
                     AND json_valid(part.data) \
                     AND (json_extract(part.data, '$.type') = 'tool' \
                       OR (json_extract(part.data, '$.type') IN ('text', 'reasoning') \
                         AND trim(coalesce(json_extract(part.data, '$.text'), '')) != ''))) \
                 ORDER BY time_created, id"
            }
            Schema::V2 => {
                // Guarded like `text_messages_v2`.
                "SELECT json_extract(m.data, '$.model.id') FROM session_message m \
                 WHERE m.session_id = ?1 AND m.type = 'assistant' \
                   AND CASE WHEN json_valid(m.data) THEN \
                         json_type(m.data, '$.model.id') END = 'text' \
                   AND EXISTS (SELECT 1 FROM json_each( \
                       CASE WHEN json_valid(m.data) THEN \
                         CASE WHEN json_type(m.data, '$.content') = 'array' \
                              THEN m.data END END, '$.content') c \
                     WHERE CASE WHEN c.type = 'object' THEN \
                       CASE json_extract(c.value, '$.type') \
                         WHEN 'tool' THEN 1 \
                         WHEN 'text' THEN json_type(c.value, '$.text') = 'text' \
                         WHEN 'reasoning' THEN json_type(c.value, '$.text') = 'text' END \
                       AND (json_extract(c.value, '$.type') = 'tool' \
                         OR trim(json_extract(c.value, '$.text')) != '') END) \
                 ORDER BY m.seq, m.id"
            }
        })?;
        let mut rows = stmt.query([id])?;
        let mut models = super::common::ModelList::default();
        while let Some(row) = rows.next()? {
            if let Some(model) = row.get::<_, Option<String>>(0)? {
                models.push(&model);
            }
        }
        Ok(models.non_empty())
    })
}

/// Search texts of an exported session (`export_session`): each text of a
/// conversation message, and the name, input, output and error of its tool
/// calls. Compaction summaries are skipped like in `text_messages`.
fn search_texts_of_export(data: &[u8], visit: &mut dyn FnMut(&str) -> bool) {
    super::common::for_each_jsonl_value_bytes(data, |line| {
        visit_search_record(line, &mut |_, text| visit(text))
    });
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
        "user" => Some(Message::user(text).at(info.pointer("/time/created"))),
        "assistant" => Some(Message::assistant(text).at(info.pointer("/time/created"))),
        _ => None,
    }
}

impl AgentPlugin for OpencodePlugin {
    fn clear_caches(&self) {
        if let Ok(mut c) = SESSION_CACHE.lock() {
            c.clear();
        }
    }

    fn id(&self) -> &'static str {
        "opencode"
    }

    fn description(&self) -> &'static str {
        "opencode"
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn can_memory(&self) -> bool {
        true
    }

    fn global_memory_sources(&self, home: &Path) -> Vec<MemorySource> {
        let config = opencode_config_dir(home);
        let mut sources = vec![
            MemorySource::new(&config, "AGENTS.md", MemoryKind::Instruction),
            MemorySource::new(&config, "skills/*/SKILL.md", MemoryKind::Skill),
        ];
        sources.extend(configured_instructions(&config, home));
        sources
    }

    fn project_memory_sources(&self, dir: &Path) -> Vec<MemorySource> {
        let mut sources = vec![MemorySource::new(
            dir,
            ".opencode/skills/*/SKILL.md",
            MemoryKind::Skill,
        )];
        sources.extend(configured_instructions(
            dir,
            &super::common::canonical_home(),
        ));
        sources
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
            let table = Schema::of(conn)?.session_table();
            let archived_col = time_archived_expr(conn, table)?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, parent_id, directory, title, time_created, time_updated, {archived_col} \
                 FROM {table}"
            ))?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>("id")?, row_to_session(row)?))
            })?;
            // Skip malformed rows instead of dropping every session.
            let mut ok = Vec::new();
            for row in rows {
                if crate::cancel::checkpoint() {
                    return Ok(Vec::new());
                }
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

    fn iter_search_records(&self, path: &Path, visit: &mut RecordVisitor<'_>) {
        if let Some(data) = self.session_bytes(path) {
            super::common::for_each_jsonl_value_bytes(&data, |val| {
                visit.record(|emit| visit_search_record(val, emit))
            });
        }
    }

    fn iter_search_texts(&self, path: &Path, visit: &mut dyn FnMut(&str) -> bool) {
        if let Some(bytes) = self.session_bytes(path) {
            search_texts_of_export(&bytes, visit);
        }
    }

    fn iter_search_texts_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(&str) -> bool,
    ) {
        search_texts_of_export(data, visit);
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

    fn resolve_models(&self, path: &Path) -> Option<super::common::ModelList> {
        let (db, id) = split_virtual_path(path)?;
        session_row(path)?;
        session_models(db, id).map_err(|e| warn_once(db, &e)).ok()?
    }

    fn is_archived(&self, path: &Path) -> bool {
        session_row(path).is_some_and(|row| row.archived)
    }

    fn parent_session_id(&self, path: &Path) -> Option<String> {
        session_row(path)?.parent_id.filter(|id| !id.is_empty())
    }
}

/// Search texts of an opencode 2 export line. Only `user` and `assistant`
/// rows are conversation; reasoning is not searched, as in 1.x. A tool's
/// output is the text items of `state.content` (the others are files, e.g.
/// an image as a data URI) and the `message` of `state.error`.
fn visit_search_record_v2(
    kind: Option<&str>,
    data: &serde_json::Value,
    visit: &mut TypedVisitor<'_>,
) -> bool {
    match kind {
        Some("user") => data
            .get("text")
            .and_then(|v| v.as_str())
            .is_none_or(|text| visit(K::User, text)),
        Some("assistant") => {
            let items = data.get("content").and_then(|v| v.as_array());
            items.into_iter().flatten().all(|item| {
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("text") => item
                        .get("text")
                        .and_then(|v| v.as_str())
                        .is_none_or(|text| visit(K::Assistant, text)),
                    Some("tool") => {
                        let state = item.get("state");
                        super::common::visit_tool_call(
                            item.get("name"),
                            state.and_then(|s| s.get("input")),
                            &mut |text| visit(K::ToolInput, text),
                        ) && {
                            let output = state.and_then(|s| s.get("content"));
                            let output = output.and_then(|v| v.as_array()).into_iter().flatten();
                            let error = state.and_then(|s| s.get("error"));
                            output
                                .filter(|part| {
                                    part.get("type").and_then(|v| v.as_str()) == Some("text")
                                })
                                .map(|part| part.get("text"))
                                .chain([error.map(|e| e.get("message").unwrap_or(e))])
                                .all(|text| {
                                    text.and_then(|v| v.as_str())
                                        .is_none_or(|text| visit(K::ToolOutput, text))
                                })
                        }
                    }
                    _ => true,
                }
            })
        }
        _ => true,
    }
}

fn visit_search_record(line: &serde_json::Value, visit: &mut TypedVisitor<'_>) -> bool {
    if let Some(data) = line.get("data") {
        return visit_search_record_v2(line.get("type").and_then(|v| v.as_str()), data, visit);
    }
    if line.pointer("/info/summary").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    let Some(parts) = line.get("parts").and_then(|v| v.as_array()) else {
        return true;
    };
    parts
        .iter()
        .all(|part| match part.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                let injected = ["synthetic", "ignored"]
                    .iter()
                    .any(|key| part.get(*key).and_then(|v| v.as_bool()) == Some(true));
                injected
                    || part
                        .get("text")
                        .and_then(|v| v.as_str())
                        .is_none_or(|text| {
                            visit(
                                match line.pointer("/info/role").and_then(|v| v.as_str()) {
                                    Some("user") => K::User,
                                    Some("assistant") => K::Assistant,
                                    _ => K::Unknown,
                                },
                                text,
                            )
                        })
            }
            Some("tool") => {
                let state = part.get("state");
                super::common::visit_tool_call(
                    part.get("tool"),
                    state.and_then(|s| s.get("input")),
                    &mut |text| visit(K::ToolInput, text),
                ) && ["output", "error"].iter().all(|key| {
                    state.and_then(|s| s.get(*key)).is_none_or(|v| {
                        super::common::visit_tool_output(v, &mut |text| visit(K::ToolOutput, text))
                    })
                })
            }
            _ => true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_jsonc_removes_comments_and_trailing_commas() {
        let src = "{\n  // line\n  \"a\": \"x//y /* z */\", /* block */\n  \"b\": [1, 2,],\n}\n";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["a"], "x//y /* z */");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
    }

    #[test]
    fn configured_instructions_resolve_relative_home_and_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"instructions":["docs/*.md","~/g.md","/abs/x.md","https://e.com/r.md"]}"#,
        )
        .unwrap();
        let home = Path::new("/home/u");
        let patterns: Vec<String> = configured_instructions(dir, home)
            .iter()
            .map(MemorySource::pattern)
            .collect();
        assert_eq!(
            patterns,
            vec![
                format!("{}/docs/*.md", dir.display()),
                "/home/u/g.md".to_string(),
                "/abs/x.md".to_string(),
            ]
        );
    }

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
    fn expands_child_sessions_as_subagents() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let mut sessions = PLUGIN.expand_sessions(&db).unwrap();
        sessions.sort();
        assert_eq!(
            sessions,
            vec![
                (db.join("ses_aaa"), millis_to_system_time(1700000100000)),
                (db.join("ses_child"), millis_to_system_time(1700000200000)),
                (db.join("ses_empty"), millis_to_system_time(1700000000000)),
            ]
        );
        assert!(PLUGIN.is_subagent(&db.join("ses_child")));
        assert_eq!(
            PLUGIN.parent_session_id(&db.join("ses_child")).as_deref(),
            Some("ses_aaa")
        );
        assert!(!PLUGIN.is_subagent(&db.join("ses_aaa")));
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
            // `time.created` of the message, where the fixture has one
            Message::user("hello there".to_string()).at(Some(&1.into())),
            Message::assistant("hi back".to_string()).at(Some(&3.into())),
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
    fn missing_sessions_do_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        assert_eq!(PLUGIN.session_mtime(&db.join("ses_nope")), None);
        assert_eq!(
            PLUGIN.session_mtime(&db.join("ses_child")),
            Some(millis_to_system_time(1700000200000))
        );
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
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 3);
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
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 4);
    }

    #[test]
    fn active_writer_is_read_through_its_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 3);
        // A writer keeps its connection open: the new row lives in the -wal.
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch(
                "INSERT INTO session VALUES ('ses_wal', 'p', NULL, 'w', '/x', 'Wal', '1', 1, 1);",
            )
            .unwrap();
        assert!(db.with_file_name("opencode.db-wal").exists());
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 4);
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
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 3);
    }

    #[test]
    fn uri_path_escapes_special_characters() {
        assert_eq!(
            uri_path(Path::new("/a b/c?d#e%f.db")),
            "/a%20b/c%3Fd%23e%25f.db"
        );
    }

    #[test]
    fn model_lists_assistant_model_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db(tmp.path());
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            UPDATE message SET data = '{"role":"user","model":{"modelID":"not-this"}}'
                WHERE id = 'msg_1';
            UPDATE message SET data = '{"role":"assistant","modelID":"kimi-k2.7-code"}'
                WHERE id = 'msg_2';
            UPDATE message SET data = '{"role":"assistant","modelID":"deepseek-v4-pro"}'
                WHERE id = 'msg_3';
            UPDATE message SET data = '{"role":"assistant","summary":true,"modelID":"compactor"}'
                WHERE id = 'msg_4';
            INSERT INTO message VALUES
                ('msg_6', 'ses_aaa', 6, 6, '{"role":"assistant","modelID":"interrupted"}'),
                ('msg_7', 'ses_aaa', 7, 7, '{"role":"assistant","modelID":123}'),
                ('msg_8', 'ses_aaa', 8, 8, '{"role":"assistant","modelID":"empty-reasoning"}');
            INSERT INTO part VALUES
                ('prt_9', 'msg_6', 'ses_aaa', 6, 6, '{"type":"step-start"}'),
                ('prt_10', 'msg_7', 'ses_aaa', 7, 7, '{"type":"text","text":"x"}'),
                ('prt_11', 'msg_8', 'ses_aaa', 8, 8, '{"type":"reasoning","text":""}');
            "#,
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            PLUGIN
                .resolve_models(&db.join("ses_aaa"))
                .map(|m| m.joined())
                .as_deref(),
            Some("kimi-k2.7-code,deepseek-v4-pro")
        );
        assert!(PLUGIN.resolve_models(&db.join("ses_empty")).is_none());
        assert!(PLUGIN.resolve_models(&db.join("ses_missing")).is_none());
    }

    /// Turn `create_db` into what opencode 2 leaves after upgrading it: the
    /// 1.x tables stay, and the sessions live in `session_v2` /
    /// `session_message`. `ses_aaa` was migrated; `ses_two` is new, with no
    /// title yet, and exists only in the new tables.
    fn create_db_v2(dir: &Path) -> PathBuf {
        let db = create_db(dir);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
                parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
                title TEXT, version TEXT NOT NULL, model TEXT,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
            CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL);
            CREATE TABLE session_part (message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                ordinal INTEGER NOT NULL, type TEXT NOT NULL, call_id TEXT,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL, PRIMARY KEY (message_id, ordinal));
            INSERT INTO session_v2 VALUES
                ('ses_aaa', 'p', NULL, 'a', '/data/home/me/proj', 'My title', '1', NULL,
                 1700000000000, 1700000100000),
                ('ses_two', 'p', NULL, 't', '/data/home/me/two', NULL, '2.3.2', NULL,
                 1700000300000, 1700000400000),
                ('ses_kid', 'p', 'ses_two', 'k', '/data/home/me/two', 'Kid', '2.3.2', NULL,
                 1700000300000, 1700000350000);
            -- `seq` orders the rows; it has gaps and follows neither id nor time.
            INSERT INTO session_message VALUES
                ('msg_z', 'ses_two', 'user', 4, 9, 9,
                 '{"time":{"created":1700000300000},"text":" first prompt ","files":[]}'),
                ('msg_y', 'ses_two', 'system', 5, 8, 8, '{"text":"tools changed"}'),
                ('msg_x', 'ses_two', 'assistant', 6, 7, 7,
                 '{"time":{"created":1700000301000},"model":{"id":"model-a","providerID":"p"},"content":[{"type":"reasoning","text":"thinking-v2"},{"type":"text","text":"one"},{"type":"tool","id":"c1","name":"shell","state":{"status":"completed","input":{"command":"ls needle-input"},"content":[{"type":"text","text":"needle-output"},{"type":"file","uri":"data:image/png;base64,needle-image","mime":"image/png"}]}},{"type":"tool","id":"c2","name":"read","state":{"status":"error","input":{},"error":{"type":"tool.execution","message":"needle-error"}}},"stray",{"type":"text","text":{"not":"a string"}},{"type":"text","text":"two"}]}'),
                ('msg_w', 'ses_two', 'synthetic', 7, 6, 6, '{"text":"Continue if you have next steps"}'),
                ('msg_v', 'ses_two', 'compaction', 8, 5, 5, '{"status":"completed","summary":"compacted"}'),
                ('msg_u', 'ses_two', 'assistant', 9, 4, 4,
                 '{"model":{"id":"interrupted"},"content":[{"type":"reasoning","text":""}]}'),
                ('msg_t', 'ses_two', 'assistant', 10, 3, 3,
                 '{"model":{"id":"model-b"},"content":[{"type":"tool","id":"c3","name":"read","state":{}}]}'),
                ('msg_s', 'ses_two', 'assistant', 11, 2, 2,
                 '{"model":{"id":7},"content":[{"type":"text","text":"numbered"}]}'),
                ('msg_r', 'ses_two', 'assistant', 12, 1, 1, '{"model":{"id":"no-content"},"content":"x"}'),
                ('msg_q', 'ses_two', 'user', 13, 1, 1, '{"text":"second prompt"}'),
                ('msg_p', 'ses_two', 'idle', 20, 1, 1, '{"outcome":"succeeded"}'),
                ('msg_o', 'ses_two', 'user', 21, 1, 1, 'not json'),
                ('msg_n', 'ses_two', 'assistant', 22, 1, 1, 'not json'),
                ('msg_m', 'ses_two', 'user', 23, 1, 1, '{"text":{"not":"a prompt"}}'),
                ('msg_l', 'ses_two', 'assistant', 24, 1, 1,
                 '{"model":{"id":"not-an-array"},"content":{"a":{"type":"text","text":"not an item"}}}'),
                ('msg_k', 'ses_two', 'assistant', 25, 1, 1,
                 '{"model":{"id":"not-a-text"},"content":[{"type":"text","text":["x"]}]}'),
                ('msg_j', 'ses_two', 'assistant', 26, 1, 1, '["not an object"]');
            "#,
        )
        .unwrap();
        drop(conn);
        db
    }

    #[test]
    fn v2_lists_sessions_from_session_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db_v2(tmp.path());
        let mut sessions = PLUGIN.expand_sessions(&db).unwrap();
        sessions.sort();
        // `ses_child` and `ses_empty` are only in the 1.x table.
        assert_eq!(
            sessions,
            vec![
                (db.join("ses_aaa"), millis_to_system_time(1700000100000)),
                (db.join("ses_kid"), millis_to_system_time(1700000350000)),
                (db.join("ses_two"), millis_to_system_time(1700000400000)),
            ]
        );
        let path = db.join("ses_two");
        let home = Path::new("/data/home/me");
        assert_eq!(PLUGIN.resolve_title(&path, home), None);
        assert_eq!(PLUGIN.resolve_project(&path, home).as_deref(), Some("two"));
        assert_eq!(
            PLUGIN.session_created(&path),
            Some(millis_to_system_time(1700000300000))
        );
        assert_eq!(
            PLUGIN.parent_session_id(&db.join("ses_kid")).as_deref(),
            Some("ses_two")
        );
        PLUGIN.clear_caches();
        assert_eq!(
            PLUGIN.session_mtime(&path),
            Some(millis_to_system_time(1700000400000))
        );
        assert_eq!(PLUGIN.session_mtime(&db.join("ses_child")), None);
    }

    #[test]
    fn v2_parses_user_and_assistant_rows_in_seq_order() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db_v2(tmp.path());
        let path = db.join("ses_two");
        let expected = vec![
            Message::user("first prompt".to_string()).at(Some(&1700000300000i64.into())),
            Message::assistant("one\n\ntwo".to_string()).at(Some(&1700000301000i64.into())),
            Message::assistant("numbered".to_string()),
            Message::user("second prompt".to_string()),
        ];
        assert_eq!(collect_messages(&path), expected);
        let bytes = PLUGIN.session_bytes(&path).unwrap();
        let mut from_bytes = Vec::new();
        PLUGIN.iter_messages_from_bytes(&path, &bytes, &mut |m| {
            from_bytes.push(m);
            true
        });
        assert_eq!(from_bytes, expected);
        // A migrated session reads from the new tables, where it has no rows
        // in this fixture.
        assert!(collect_messages(&db.join("ses_aaa")).is_empty());
    }

    #[test]
    fn v2_session_bytes_hold_every_row_and_match_size() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db_v2(tmp.path());
        let path = db.join("ses_two");
        let bytes = PLUGIN.session_bytes(&path).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(text.lines().count(), 17);
        assert_eq!(
            text.lines().next(),
            Some(
                r#"{"id":"msg_z","type":"user","data":{"time":{"created":1700000300000},"text":" first prompt ","files":[]}}"#
            )
        );
        assert_eq!(PLUGIN.raw_content(&path).as_deref(), Some(text));
        assert_eq!(PLUGIN.session_size(&path), Some(bytes.len() as u64));
        assert_eq!(PLUGIN.session_size(&db.join("ses_aaa")), Some(0));
        assert!(PLUGIN.session_bytes(&db.join("ses_aaa")).is_none());
    }

    #[test]
    fn v2_searches_conversation_text_and_tool_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db_v2(tmp.path());
        let mut texts = Vec::new();
        PLUGIN.iter_search_texts(&db.join("ses_two"), &mut |text| {
            texts.push(text.to_string());
            true
        });
        assert_eq!(
            texts,
            vec![
                " first prompt ",
                "one",
                "shell",
                "ls needle-input",
                "needle-output",
                "read",
                "needle-error",
                "two",
                "read",
                "numbered",
                "second prompt",
            ]
        );
    }

    #[test]
    fn v2_model_lists_answering_model_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db_v2(tmp.path());
        assert_eq!(
            PLUGIN
                .resolve_models(&db.join("ses_two"))
                .map(|m| m.joined())
                .as_deref(),
            Some("model-a,model-b")
        );
        assert!(PLUGIN.resolve_models(&db.join("ses_aaa")).is_none());
    }

    #[test]
    fn session_v2_without_session_message_reads_the_1x_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db_v2(tmp.path());
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE session_message;").unwrap();
        drop(conn);
        assert_eq!(PLUGIN.expand_sessions(&db).unwrap().len(), 3);
        assert_eq!(collect_messages(&db.join("ses_aaa")).len(), 3);
        assert_eq!(
            PLUGIN.session_mtime(&db.join("ses_child")),
            Some(millis_to_system_time(1700000200000))
        );
    }

    #[test]
    fn archived_sessions_are_detected_from_time_archived() {
        let tmp = tempfile::tempdir().unwrap();
        let db = create_db_v2(tmp.path());
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("ALTER TABLE session_v2 ADD COLUMN time_archived INTEGER;")
            .unwrap();
        conn.execute(
            "UPDATE session_v2 SET time_archived = 1700000500000 WHERE id = 'ses_aaa'",
            [],
        )
        .unwrap();
        drop(conn);

        PLUGIN.clear_caches();
        assert!(PLUGIN.is_archived(&db.join("ses_aaa")));
        assert!(!PLUGIN.is_archived(&db.join("ses_two")));
    }
}

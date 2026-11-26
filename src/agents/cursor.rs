use super::entries::{RecordVisitor, SearchKind as K, TypedVisitor};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use regex::Regex;
use rusqlite::{Connection, OpenFlags};

use super::AgentPlugin;
use super::Message;
use super::common::first_text_part;
use super::common::for_each_jsonl_value_bytes;
use super::common::is_safe_cli_id;
use super::common::mmap_file;
use super::common::tagged_user_body;
use super::common::{visit_text_parts, visit_tool_call};
use super::{MemoryKind, MemorySource};

/// Visit the message of one transcript line; `false` stops the iteration.
fn visit_message_value(val: &serde_json::Value, visit: &mut dyn FnMut(Message) -> bool) -> bool {
    match val.get("role").and_then(|v| v.as_str()) {
        Some("user") => {
            if let Some(raw) = val.get("message").and_then(first_text_part)
                && let Some(text) = cursor_user_body(raw)
            {
                return visit(Message::user(text.to_string()));
            }
        }
        Some("assistant") => {
            if let Some(text) = val.get("message").and_then(first_text_part) {
                return visit(Message::assistant(text.to_string()));
            }
        }
        _ => {}
    }
    true
}

/// Parent chat id of a subagent transcript
/// (`agent-transcripts/<parent id>/subagents/<id>.jsonl`).
fn subagent_parent(path: &Path) -> Option<&str> {
    let dir = path.parent()?;
    if dir.file_name()? != "subagents" {
        return None;
    }
    let parent = dir.parent()?;
    if parent.parent()?.file_name()? != "agent-transcripts" {
        return None;
    }
    parent.file_name()?.to_str()
}

static RE_CURSOR_PROJECTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".*/projects/([^/]+)/.*").unwrap());

static DECODE_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Child names per directory, shared by every decode in this process so that
/// common ancestors (`/`, the home directory, ...) are listed only once.
/// `None` records a directory that could not be listed.
type DirListing = Option<Arc<Vec<OsString>>>;
static LISTING_CACHE: LazyLock<Mutex<HashMap<PathBuf, DirListing>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Decode Cursor's dash-encoded directory name to an actual filesystem path.
/// e.g. "Users-you-src-github-com-org-repo" -> "/Users/you/src/github.com/org/repo"
/// Results are cached per encoded string.
fn decode_cursor_path(encoded: &str) -> Option<String> {
    if let Ok(cache) = DECODE_CACHE.lock()
        && let Some(result) = cache.get(encoded)
    {
        return result.clone();
    }
    let result = decode_cursor_path_inner(Path::new("/"), encoded);
    if let Ok(mut cache) = DECODE_CACHE.lock() {
        cache.insert(encoded.to_string(), result.clone());
    }
    result
}

/// Encode one path component the way Cursor does: every run of characters
/// other than ASCII letters and digits becomes a single `-`, and leading or
/// trailing dashes are dropped (so `.claude` → `claude`, `pce_flutter` →
/// `pce-flutter`, `日本語` → ``).
fn encode_cursor_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Resolve `encoded` against the directories that exist under `root`.
///
/// The encoding is lossy (`/`, `.`, `_` and `-` all become `-`), so the path
/// is rebuilt by walking the filesystem: at each level, a child directory
/// whose encoded name is a dash-bounded prefix of the remaining string is
/// descended into, backtracking on dead ends. The longest match is tried
/// first, so when both `foo-bar` and `foo/bar` exist as complete matches,
/// `foo-bar` wins; the encoding cannot tell them apart.
///
/// When the directory no longer exists (e.g. a removed worktree), the deepest
/// existing ancestor is kept and the undecodable rest is appended as a single
/// component so that the project name stays readable. When not even the first
/// component exists (a tree from another machine), every `-` is read as `/`.
fn decode_cursor_path_inner(root: &Path, encoded: &str) -> Option<String> {
    let encoded = encoded.trim_matches('-');
    if encoded.is_empty() {
        return None;
    }
    let mut best = (0, root.to_path_buf());
    // Names that encode to nothing can hide a large subtree, so they are
    // only entered when the ordinary walk finds nothing.
    for zero_width in [false, true] {
        let mut walk = Walk {
            total: encoded.len(),
            best,
            seen: HashMap::new(),
            budget: MAX_DECODE_VISITS,
            zero_width,
        };
        if let Some(found) = walk.descend(root, encoded, 0) {
            // Resolve symlinks (e.g. `/home/user` → `/data/home/user`) so the
            // result matches the canonicalized cwd filter.
            let found = std::fs::canonicalize(&found).unwrap_or(found);
            return Some(found.to_string_lossy().into_owned());
        }
        best = walk.best;
    }
    let (consumed, dir) = best;
    if consumed == 0 {
        return Some(
            root.join(encoded.replace('-', "/"))
                .to_string_lossy()
                .into_owned(),
        );
    }
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let rest = encoded[consumed..].trim_start_matches('-');
    Some(dir.join(rest).to_string_lossy().into_owned())
}

/// Guard against runaway recursion through components that consume no input
/// (names without ASCII alphanumerics). Ordinary components always consume
/// input, so their depth is bounded by the slug length.
const MAX_ZERO_WIDTH_DEPTH: usize = 256;
/// Upper bound on directories entered per decode.
const MAX_DECODE_VISITS: usize = 10_000;
/// Tokens probed per level when a directory cannot be listed (640 names).
const MAX_PROBE_TOKENS: usize = 4;

struct Walk {
    total: usize,
    /// Deepest existing directory reached: (bytes of input consumed, path).
    best: (usize, PathBuf),
    /// (directory identity, remaining length) states already explored, with
    /// the fewest zero-width components they were entered through.
    seen: HashMap<(DirId, usize), usize>,
    budget: usize,
    /// Whether names without ASCII alphanumerics may be entered.
    zero_width: bool,
}

#[cfg(unix)]
type DirId = (u64, u64);
#[cfg(not(unix))]
type DirId = PathBuf;

#[cfg(unix)]
fn dir_id(_path: &Path, meta: &std::fs::Metadata) -> Option<DirId> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn dir_id(path: &Path, _meta: &std::fs::Metadata) -> Option<DirId> {
    std::fs::canonicalize(path).ok()
}

fn list_dir(dir: &Path) -> DirListing {
    if let Ok(cache) = LISTING_CACHE.lock()
        && let Some(listing) = cache.get(dir)
    {
        return listing.clone();
    }
    let listing = std::fs::read_dir(dir)
        .ok()
        .map(|entries| Arc::new(entries.flatten().map(|e| e.file_name()).collect()));
    if let Ok(mut cache) = LISTING_CACHE.lock() {
        cache.insert(dir.to_path_buf(), listing.clone());
    }
    listing
}

/// Candidate names for a directory that can be searched but not listed
/// (mode `--x`): the next few tokens joined by every combination of the
/// separators Cursor folds into `-`, with and without a leading or trailing
/// separator (which the encoding trims). Only single `.`, `_` or `-`
/// separators are tried; runs such as `__tests__` or other symbols are not.
fn probe_names(rest: &str) -> Vec<OsString> {
    let tokens: Vec<&str> = rest.split('-').take(MAX_PROBE_TOKENS).collect();
    let mut names = Vec::new();
    let mut prefixes = vec![tokens[0].to_string()];
    for (i, token) in tokens.iter().enumerate() {
        if i > 0 {
            prefixes = prefixes
                .iter()
                .flat_map(|p| ["-", "_", "."].map(|sep| format!("{p}{sep}{token}")))
                .collect();
        }
        for name in &prefixes {
            for lead in ["", ".", "_", "-"] {
                for trail in ["", ".", "_", "-"] {
                    names.push(OsString::from(format!("{lead}{name}{trail}")));
                }
            }
        }
    }
    names
}

impl Walk {
    fn descend(&mut self, dir: &Path, rest: &str, zero_width_depth: usize) -> Option<PathBuf> {
        if zero_width_depth >= MAX_ZERO_WIDTH_DEPTH || self.budget == 0 {
            return None;
        }
        self.budget -= 1;
        let names = list_dir(dir).unwrap_or_else(|| Arc::new(probe_names(rest)));
        let mut candidates: Vec<(String, PathBuf, Option<DirId>)> = names
            .iter()
            .filter_map(|name| {
                let enc = encode_cursor_component(&name.to_string_lossy());
                // A name without ASCII alphanumerics (`日本語`, `_`) leaves no
                // trace in the encoding: it consumes nothing.
                let matches = (enc.is_empty() && self.zero_width)
                    || (!enc.is_empty()
                        && rest.starts_with(&enc)
                        && matches!(rest.as_bytes().get(enc.len()), None | Some(b'-')));
                if !matches {
                    return None;
                }
                // `metadata` follows symlinks, so `/home/user` → `/data/home/user`
                // counts. Continue from the resolved path so that chains of
                // symlinks never pile up (and hit the kernel's loop limit).
                let path = dir.join(name);
                let meta = std::fs::metadata(&path).ok().filter(|m| m.is_dir())?;
                let path = std::fs::canonicalize(&path).unwrap_or(path);
                let id = dir_id(&path, &meta);
                Some((enc, path, id))
            })
            .collect();
        // Longest match first (fewest components), zero-width names last;
        // ties broken by path for deterministic output.
        candidates.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.1.cmp(&b.1)));
        for (enc, path, id) in candidates {
            let remaining = rest[enc.len()..].trim_start_matches('-');
            if remaining.is_empty() && !enc.is_empty() {
                return Some(path);
            }
            let zero_width_depth = zero_width_depth + usize::from(enc.is_empty());
            // A state explored before has failed (success returns at once),
            // unless the zero-width depth limit cut that attempt shorter.
            if let Some(id) = id {
                let seen = self.seen.entry((id, remaining.len())).or_insert(usize::MAX);
                if *seen <= zero_width_depth {
                    continue;
                }
                *seen = zero_width_depth;
            }
            let consumed = self.total - remaining.len();
            if consumed > self.best.0 {
                self.best = (consumed, path.clone());
            }
            if let Some(found) = self.descend(&path, remaining, zero_width_depth) {
                return Some(found);
            }
        }
        None
    }
}

pub static PLUGIN: CursorPlugin = CursorPlugin;

pub struct CursorPlugin;

/// Cursor transcripts wrap real user text in `<user_query>…</user_query>`; strip for title / prompts.
fn cursor_user_body(raw: &str) -> Option<&str> {
    tagged_user_body(raw, "user_query")
}

impl AgentPlugin for CursorPlugin {
    fn clear_caches(&self) {
        if let Ok(mut c) = DECODE_CACHE.lock() {
            c.clear();
        }
        if let Ok(mut c) = LISTING_CACHE.lock() {
            c.clear();
        }
        if let Ok(mut c) = CHAT_STORE_INDEX_CACHE.lock() {
            c.clear();
        }
        if let Ok(mut c) = CHAT_META_CACHE.lock() {
            c.clear();
        }
    }

    fn id(&self) -> &'static str {
        "cursor"
    }

    fn description(&self) -> &'static str {
        "Cursor Agent"
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

    fn running_sessions(&self) -> Vec<(String, Option<u32>)> {
        let Some(chats) =
            crate::config::resolve_agent_base("cursor").map(|base| base.join("chats"))
        else {
            return Vec::new();
        };
        if !chats.is_dir() {
            return Vec::new();
        }
        running_in(&chats, &super::common::sqlite_wal_holders(), |pid| {
            super::common::process_args(pid).is_some_and(|args| is_cursor_agent(&args))
        })
    }

    fn can_memory(&self) -> bool {
        true
    }

    /// User rules and skills live in `$HOME/.cursor/` even when
    /// `CURSOR_DATA_DIR` moves the session data.
    fn global_memory_sources(&self, home: &Path) -> Vec<MemorySource> {
        let base = home.join(".cursor");
        vec![
            MemorySource::new(&base.join("rules"), "**/*.mdc", MemoryKind::Rule),
            MemorySource::new(&base, "skills/*/SKILL.md", MemoryKind::Skill),
        ]
    }

    fn project_memory_sources(&self, dir: &Path) -> Vec<MemorySource> {
        vec![
            MemorySource::new(dir, ".cursorrules", MemoryKind::Instruction),
            MemorySource::new(dir, ".cursor/rules/**/*.mdc", MemoryKind::Rule),
            MemorySource::new(dir, ".cursor/skills/*/SKILL.md", MemoryKind::Skill),
        ]
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: cwd path encoded in session dir name)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".cursor/projects/*/agent-transcripts/*.jsonl",
            ".cursor/projects/*/agent-transcripts/*/*.jsonl",
        ]
    }

    fn subagent_glob_patterns(&self) -> &'static [&'static str] {
        &[".cursor/projects/*/agent-transcripts/*/subagents/*.jsonl"]
    }

    fn parent_session_id(&self, path: &Path) -> Option<String> {
        subagent_parent(path).map(str::to_string)
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.cursor/"]
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
        for_each_jsonl_value_bytes(data, |val| visit_message_value(val, visit));
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

    /// Messages plus `tool_use` names and inputs. Transcripts do not record
    /// tool results.
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
        let path_str = path.to_string_lossy();
        let caps = RE_CURSOR_PROJECTS.captures(&path_str)?;
        decode_cursor_path(&caps[1])
    }

    fn resolve_project(&self, path: &Path, _home: &Path) -> Option<String> {
        let path_str = path.to_string_lossy();
        RE_CURSOR_PROJECTS
            .captures(&path_str)
            .and_then(|caps| decode_cursor_path(&caps[1]))
            .or_else(|| Some("?".to_string()))
    }

    fn resolve_title(&self, path: &Path, home: &Path) -> Option<String> {
        chat_meta_for(path, home)?.title
    }

    fn resolve_models(&self, path: &Path) -> Option<super::common::ModelList> {
        let model = chat_meta_for(path, Path::new(""))?.model?;
        let mut list = super::common::ModelList::default();
        list.push(&model);
        Some(list)
    }

    // Subagent transcripts are not resumable chats of their own.
    fn resolve_resume_id(&self, path: &Path, home: &Path) -> Option<String> {
        if subagent_parent(path).is_some() {
            return None;
        }
        self.session_id(path, home)
    }

    fn session_id(&self, path: &Path, _home: &Path) -> Option<String> {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .filter(|id| is_safe_cli_id(id))
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["cursor-agent".to_string(), "--resume".to_string(), id])
    }
}

#[derive(Clone, Default)]
struct ChatMeta {
    title: Option<String>,
    model: Option<String>,
}

type FileStamp = Option<(SystemTime, u64)>;

fn file_stamp(path: &Path) -> FileStamp {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

struct CachedStoreIndex {
    stamp: (Option<SystemTime>, usize, Option<SystemTime>),
    by_id: Arc<HashMap<String, PathBuf>>,
}

struct CachedChatMeta {
    stamp: [FileStamp; 2],
    meta: ChatMeta,
}

static CHAT_STORE_INDEX_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedStoreIndex>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static CHAT_META_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedChatMeta>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Walk up from `<base>/projects/<encoded>/agent-transcripts/...` to `<base>`.
fn cursor_data_base(path: &Path) -> Option<PathBuf> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.file_name().and_then(|s| s.to_str()) == Some("projects") {
            return dir.parent().map(Path::to_path_buf);
        }
        current = dir.parent();
    }
    None
}

/// Stamp covering `chats/` and all immediate `<hash>` subdirectories so adding
/// a new `<hash>` or `<hash>/<id>` directory invalidates the cached index.
fn chats_dir_stamp(chats_dir: &Path) -> (Option<SystemTime>, usize, Option<SystemTime>) {
    let root_mtime = fs::metadata(chats_dir).and_then(|m| m.modified()).ok();
    let mut count = 0usize;
    let mut max_sub_mtime: Option<SystemTime> = None;
    if let Ok(entries) = fs::read_dir(chats_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata()
                && meta.is_dir()
            {
                count += 1;
                if let Ok(m) = meta.modified() {
                    max_sub_mtime = Some(match max_sub_mtime {
                        Some(prev) if prev >= m => prev,
                        _ => m,
                    });
                }
            }
        }
    }
    (root_mtime, count, max_sub_mtime)
}

/// Whether `args` are those of cursor-agent: the program itself, or the
/// runtime it starts on `.../cursor-agent/versions/<version>/index.js`.
fn is_cursor_agent(args: &[String]) -> bool {
    let named = |arg: &String, name: &str| Path::new(arg).file_name().is_some_and(|n| n == name);
    let Some(program) = args.first() else {
        return false;
    };
    named(program, "cursor-agent")
        || (named(program, "node")
            && args[1..].iter().any(|arg| {
                named(arg, "index.js")
                    && Path::new(arg)
                        .components()
                        .any(|part| part.as_os_str() == "cursor-agent")
            }))
}

/// Running sessions from `chats/<hash>/<chat id>/store.db-shm`. cursor-agent
/// keeps a SQLite connection to a chat's `store.db` open for as long as the
/// chat is, and with it the shared lock every WAL connection holds on the
/// `-shm` file; the files stay behind after a crash, the lock does not.
/// Other readers of the database hold the same lock for a moment, so a
/// holder counts only when `is_cursor` says it is cursor-agent.
fn running_in(
    chats_dir: &Path,
    holders: &HashMap<(u64, u64, u64), Vec<u32>>,
    is_cursor: impl Fn(u32) -> bool,
) -> Vec<(String, Option<u32>)> {
    // The files cursor-agent holds open; most hosts have other SQLite
    // users, and none of their files is a chat.
    let mut known: HashMap<u32, bool> = HashMap::new();
    let held: HashMap<(u64, u64, u64), u32> = holders
        .iter()
        .filter_map(|(file, pids)| {
            let pid = pids
                .iter()
                .find(|pid| *known.entry(**pid).or_insert_with(|| is_cursor(**pid)))?;
            Some((*file, *pid))
        })
        .collect();
    if held.is_empty() {
        return Vec::new();
    }
    let dirs = |dir: &Path| {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
    };
    dirs(chats_dir)
        .flat_map(|hash| dirs(&hash))
        .filter_map(|chat| {
            let pid = *held.get(&super::common::file_lock_key(&chat.join("store.db-shm"))?)?;
            let id = chat.file_name()?.to_str()?;
            is_safe_cli_id(id).then(|| (id.to_string(), Some(pid)))
        })
        .collect()
}

fn chat_store_path(base: &Path, id: &str) -> Option<PathBuf> {
    let chats_dir = base.join("chats");
    if let Ok(mut cache) = CHAT_STORE_INDEX_CACHE.lock() {
        // Fast path: known chat IDs resolve in O(1) with zero filesystem I/O.
        if let Some(entry) = cache.get(base)
            && let Some(db) = entry.by_id.get(id)
        {
            return Some(db.clone());
        }
        // Only check subdirectory mtimes / rescan on a cache miss.
        let stamp = chats_dir_stamp(&chats_dir);
        if let Some(entry) = cache.get(base)
            && entry.stamp == stamp
        {
            return None;
        }
        let mut by_id = HashMap::new();
        if let Ok(hashes) = fs::read_dir(&chats_dir) {
            for hash_entry in hashes.flatten() {
                if let Ok(chats) = fs::read_dir(hash_entry.path()) {
                    for chat_entry in chats.flatten() {
                        let chat_id = chat_entry.file_name().to_string_lossy().to_string();
                        // Record the expected store.db path even if store.db has not
                        // been created inside this chat dir yet, so later creation is
                        // detected without rescanning `chats/`.
                        by_id.insert(chat_id, chat_entry.path().join("store.db"));
                    }
                }
            }
        }
        let found = by_id.get(id).cloned();
        cache.insert(
            base.to_path_buf(),
            CachedStoreIndex {
                stamp,
                by_id: Arc::new(by_id),
            },
        );
        return found;
    }
    None
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

fn read_chat_meta(db: &Path) -> ChatMeta {
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
    };
    let Ok(conn) = conn else {
        return ChatMeta::default();
    };
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let raw: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .ok();
    let Some(raw) = raw else {
        return ChatMeta::default();
    };
    let decoded = decode_hex(raw.trim()).unwrap_or_else(|| raw.into_bytes());
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
        return ChatMeta::default();
    };
    let title = val
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "New Agent")
        .map(str::to_string);
    let model = val
        .get("lastUsedModel")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default")
        .map(str::to_string);
    ChatMeta { title, model }
}

fn chat_meta_for(path: &Path, _home: &Path) -> Option<ChatMeta> {
    let base = cursor_data_base(path)?;
    let id = PLUGIN.session_id(path, _home)?;
    let db = chat_store_path(&base, &id)?;
    if !db.is_file() {
        return None;
    }
    let real = fs::canonicalize(&db).unwrap_or_else(|_| db.clone());
    let wal = PathBuf::from(format!("{}-wal", real.display()));
    let stamp = [file_stamp(&real), file_stamp(&wal)];
    if let Ok(cache) = CHAT_META_CACHE.lock()
        && let Some(entry) = cache.get(&db)
        && entry.stamp == stamp
    {
        return Some(entry.meta.clone());
    }
    // Read SQLite outside the global mutex so parallel resolvers for different
    // chats do not block each other on a single busy database.
    let meta = read_chat_meta(&db);
    if let Ok(mut cache) = CHAT_META_CACHE.lock() {
        cache.insert(
            db,
            CachedChatMeta {
                stamp,
                meta: meta.clone(),
            },
        );
    }
    Some(meta)
}

fn visit_search_record(val: &serde_json::Value, visit: &mut TypedVisitor<'_>) -> bool {
    // every text part, not only the first as in `visit_message_value`
    let texts_ok = match (val.get("role").and_then(|v| v.as_str()), val.get("message")) {
        (Some("user"), Some(message)) => visit_text_parts(message, &mut |raw| {
            cursor_user_body(raw).is_none_or(|text| visit(K::User, text))
        }),
        (Some("assistant"), Some(message)) => {
            visit_text_parts(message, &mut |text| visit(K::Assistant, text))
        }
        _ => true,
    };
    texts_ok
        && val
            .pointer("/message/content")
            .and_then(|v| v.as_array())
            .is_none_or(|items| {
                items.iter().all(|item| {
                    item.get("type").and_then(|v| v.as_str()) != Some("tool_use")
                        || visit_tool_call(item.get("name"), item.get("input"), &mut |text| {
                            visit(K::ToolInput, text)
                        })
                })
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkdirs(root: &Path, rels: &[&str]) {
        for rel in rels {
            std::fs::create_dir_all(root.join(rel)).unwrap();
        }
    }

    fn decode(root: &Path, encoded: &str) -> String {
        decode_cursor_path_inner(root, encoded).unwrap()
    }

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        (tmp, root)
    }

    #[cfg(unix)]
    #[test]
    fn running_chats_are_held_open_by_cursor_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let chats = tmp.path().join("chats");
        let shm = |id: &str| {
            let dir = chats.join("deadbeef").join(id);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("store.db-shm");
            fs::write(&path, "").unwrap();
            super::super::common::file_lock_key(&path).unwrap()
        };
        let (live, read, stale) = (shm("chat-live"), shm("chat-read"), shm("chat-stale"));
        fs::create_dir_all(chats.join("deadbeef/chat-new")).unwrap();

        let args = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert!(is_cursor_agent(&args(&[
            "/home/me/.local/bin/cursor-agent",
            "--resume",
            "x"
        ])));
        assert!(is_cursor_agent(&args(&[
            "/opt/node",
            "--use-system-ca",
            "/home/me/.local/share/cursor-agent/versions/1/index.js",
        ])));
        assert!(!is_cursor_agent(&args(&[
            "python3",
            "probe.py",
            "cursor-agent"
        ])));
        assert!(!is_cursor_agent(&args(&[
            "sqlite3",
            "/x/cursor-agent/store.db"
        ])));
        assert!(!is_cursor_agent(&args(&["node", "/srv/app/index.js"])));
        assert!(!is_cursor_agent(&[]));

        let is_cursor = |pid| pid == 4242;
        let mut holders = HashMap::new();
        assert!(running_in(&chats, &holders, is_cursor).is_empty());
        // Another reader comes first on the live chat and is alone on `read`.
        holders.insert(live, vec![7, 4242]);
        holders.insert(read, vec![7]);
        let _ = stale;
        assert_eq!(
            running_in(&chats, &holders, is_cursor),
            vec![("chat-live".to_string(), Some(4242))]
        );
    }

    #[test]
    fn encode_component_collapses_non_alphanumerics() {
        assert_eq!(encode_cursor_component(".claude"), "claude");
        assert_eq!(encode_cursor_component("pce_flutter"), "pce-flutter");
        assert_eq!(encode_cursor_component("github.com"), "github-com");
        assert_eq!(encode_cursor_component("a--b__"), "a-b");
    }

    #[test]
    fn decode_keeps_leading_data_component() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["data/home/u/src/github.com/nihen/ah", "home"]);
        assert_eq!(
            decode(&root, "data-home-u-src-github-com-nihen-ah"),
            root.join("data/home/u/src/github.com/nihen/ah")
                .to_string_lossy()
        );
    }

    #[test]
    fn decode_restores_dots_underscores_dashes_and_hidden_dirs() {
        let (_tmp, root) = temp_root();
        let rel = "src/a-d-systems/pce_flutter/.claude/worktrees/wt-pce-flutter-pr284";
        mkdirs(&root, &[rel, "src/a"]);
        assert_eq!(
            decode(
                &root,
                "src-a-d-systems-pce-flutter-claude-worktrees-wt-pce-flutter-pr284"
            ),
            root.join(rel).to_string_lossy()
        );
    }

    #[test]
    fn decode_backtracks_from_dead_ends() {
        let (_tmp, root) = temp_root();
        // `foo-bar` is the longer match but leads nowhere; `foo/bar/baz` is right.
        mkdirs(&root, &["foo-bar", "foo/bar/baz"]);
        assert_eq!(
            decode(&root, "foo-bar-baz"),
            root.join("foo/bar/baz").to_string_lossy()
        );
    }

    #[test]
    fn decode_removed_dir_keeps_deepest_existing_ancestor() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["src/chatCX-AXA"]);
        assert_eq!(
            decode(&root, "src-chatCX-AXA-worktrees-4297-chat-scroll"),
            root.join("src/chatCX-AXA/worktrees-4297-chat-scroll")
                .to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn decode_resolves_symlinked_prefix() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["data/home/u/proj"]);
        std::os::unix::fs::symlink(root.join("data/home"), root.join("home")).unwrap();
        assert_eq!(
            decode(&root, "home-u-proj"),
            root.join("data/home/u/proj").to_string_lossy()
        );
    }

    #[test]
    fn resume_args_are_interactive() {
        let args = PLUGIN
            .resume_args(
                Path::new("/h/.cursor/projects/p/agent-transcripts/abc/abc.jsonl"),
                Path::new("/h"),
            )
            .unwrap();
        assert_eq!(args, ["cursor-agent", "--resume", "abc"]);
    }

    #[test]
    fn resume_id_rejects_option_like_stems() {
        let path = Path::new("/h/.cursor/projects/p/agent-transcripts/--yolo.jsonl");
        assert_eq!(PLUGIN.resolve_resume_id(path, Path::new("/h")), None);
    }

    #[test]
    fn decode_empty_is_none() {
        let (_tmp, root) = temp_root();
        assert_eq!(decode_cursor_path_inner(&root, ""), None);
        assert_eq!(decode_cursor_path_inner(&root, "---"), None);
    }

    #[test]
    fn decode_unknown_tree_reads_dashes_as_slashes() {
        let (_tmp, root) = temp_root();
        assert_eq!(
            decode(&root, "Users-u-src-ah"),
            root.join("Users/u/src/ah").to_string_lossy()
        );
    }

    #[test]
    fn decode_prefers_longest_component_on_ambiguous_leaf() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["p/foo/bar", "p/foo-bar"]);
        assert_eq!(
            decode(&root, "p-foo-bar"),
            root.join("p/foo-bar").to_string_lossy()
        );
    }

    #[test]
    fn decode_descends_through_dirs_without_ascii_alphanumerics() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["Users/u/日本語/深い/leaf", "a/_/b"]);
        assert_eq!(
            decode(&root, "Users-u-leaf"),
            root.join("Users/u/日本語/深い/leaf").to_string_lossy()
        );
        assert_eq!(decode(&root, "a-b"), root.join("a/_/b").to_string_lossy());
    }

    #[test]
    fn decode_prefers_paths_without_zero_width_names() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["p-foo/日本/bar", "p/foo/bar"]);
        assert_eq!(
            decode(&root, "p-foo-bar"),
            root.join("p/foo/bar").to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn decode_probes_through_unlistable_parent() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, root) = temp_root();
        let names = ["my_proj.v2", ".a-b", "_c", "-d", "e_"];
        for name in names {
            mkdirs(&root, &[format!("locked/{name}/.hidden").as_str()]);
        }
        let locked = root.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o111)).unwrap();
        let got: Vec<String> = names
            .iter()
            .map(|name| {
                decode(
                    &root,
                    &format!("locked-{}-hidden", encode_cursor_component(name)),
                )
            })
            .collect();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let want: Vec<String> = names
            .iter()
            .map(|name| {
                root.join(format!("locked/{name}/.hidden"))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn decode_handles_deep_paths() {
        let (_tmp, root) = temp_root();
        let rel = vec!["a"; 300].join("/");
        mkdirs(&root, &[rel.as_str()]);
        assert_eq!(
            decode(&root, &vec!["a"; 300].join("-")),
            root.join(&rel).to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn decode_does_not_re_explore_symlink_cycles() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["P"]);
        // `a` and `a_` both encode to `a` and lead back to `P`.
        std::os::unix::fs::symlink(root.join("P"), root.join("P/a")).unwrap();
        std::os::unix::fs::symlink(root.join("P"), root.join("P/a_")).unwrap();
        let started = std::time::Instant::now();
        let got = decode(&root, &format!("P-{}missing", "a-".repeat(24)));
        assert!(got.ends_with("missing"), "{got}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn decode_reenters_states_cut_short_by_the_depth_limit() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["p/x/日本/leaf"]);
        let mut chain = root.join("p-x");
        std::fs::create_dir(&chain).unwrap();
        for _ in 0..MAX_ZERO_WIDTH_DEPTH - 2 {
            chain.push("_");
            std::fs::create_dir(&chain).unwrap();
        }
        std::os::unix::fs::symlink(root.join("p/x"), chain.join("_")).unwrap();
        assert_eq!(
            decode(&root, "p-x-leaf"),
            root.join("p/x/日本/leaf").to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn decode_follows_long_symlink_chains() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["P/leaf"]);
        // `a_` points back to `P`; `a` points to `a_`. Following either many
        // times would exceed the kernel's symlink limit on an unresolved path.
        std::os::unix::fs::symlink(".", root.join("P/a_")).unwrap();
        std::os::unix::fs::symlink("a_", root.join("P/a")).unwrap();
        assert_eq!(
            decode(&root, &format!("P-{}leaf", "a-".repeat(45))),
            root.join("P/leaf").to_string_lossy()
        );
    }

    fn hex_encode(s: &str) -> String {
        s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn resolves_title_and_model_from_chat_store_db() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(".cursor");
        let transcript_dir = base.join("projects/tmp-proj/agent-transcripts");
        fs::create_dir_all(&transcript_dir).unwrap();

        let write_store = |id: &str, json: &str| -> (PathBuf, PathBuf) {
            let db_dir = base.join("chats/deadbeef").join(id);
            fs::create_dir_all(&db_dir).unwrap();
            let db = db_dir.join("store.db");
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);")
                .unwrap();
            conn.execute("INSERT INTO meta VALUES ('0', ?1)", [hex_encode(json)])
                .unwrap();
            let transcript = transcript_dir.join(format!("{id}.jsonl"));
            fs::write(&transcript, "").unwrap();
            (transcript, db)
        };

        let (t1, db1) = write_store(
            "11111111-aaaa",
            r#"{"agentId":"11111111-aaaa","name":"Status Line","lastUsedModel":"composer-2-fast"}"#,
        );
        let (t2, _) = write_store(
            "22222222-bbbb",
            r#"{"agentId":"22222222-bbbb","name":"New Agent","lastUsedModel":"default"}"#,
        );
        let t3 = transcript_dir.join("33333333-cccc.jsonl");
        fs::write(&t3, "").unwrap();

        let home = tmp.path();
        assert_eq!(
            PLUGIN.resolve_title(&t1, home).as_deref(),
            Some("Status Line")
        );
        assert_eq!(
            PLUGIN.resolve_models(&t1).map(|m| m.joined()).as_deref(),
            Some("composer-2-fast")
        );

        // Placeholder title and default model are omitted.
        assert_eq!(PLUGIN.resolve_title(&t2, home), None);
        assert!(PLUGIN.resolve_models(&t2).is_none());

        // Missing store.db falls back cleanly, and creating store.db later
        // inside an existing chat dir is detected without clearing caches.
        assert_eq!(PLUGIN.resolve_title(&t3, home), None);
        assert!(PLUGIN.resolve_models(&t3).is_none());

        let late_dir = base.join("chats/deadbeef/44444444-dddd");
        fs::create_dir_all(&late_dir).unwrap();
        let t4 = transcript_dir.join("44444444-dddd.jsonl");
        fs::write(&t4, "").unwrap();
        assert_eq!(PLUGIN.resolve_title(&t4, home), None);
        let late_db = late_dir.join("store.db");
        let late_conn = Connection::open(&late_db).unwrap();
        late_conn
            .execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        late_conn
            .execute(
                "INSERT INTO meta VALUES ('0', ?1)",
                [hex_encode(
                    r#"{"agentId":"44444444-dddd","name":"Late DB","lastUsedModel":"composer-2-fast"}"#,
                )],
            )
            .unwrap();
        drop(late_conn);
        assert_eq!(PLUGIN.resolve_title(&t4, home).as_deref(), Some("Late DB"));
        assert_eq!(
            PLUGIN.resolve_models(&t4).map(|m| m.joined()).as_deref(),
            Some("composer-2-fast")
        );

        // Updating store.db in WAL mode invalidates the cached metadata.
        let conn = Connection::open(&db1).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = '0'",
            [hex_encode(
                r#"{"agentId":"11111111-aaaa","name":"Renamed Chat","lastUsedModel":"claude-4.6-opus"}"#,
            )],
        )
        .unwrap();
        assert_eq!(
            PLUGIN.resolve_title(&t1, home).as_deref(),
            Some("Renamed Chat")
        );
        assert_eq!(
            PLUGIN.resolve_models(&t1).map(|m| m.joined()).as_deref(),
            Some("claude-4.6-opus")
        );
        drop(conn);
    }
}

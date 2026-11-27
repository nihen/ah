//! Read-only providers using the common normalized session cache.
use super::{FOREGROUND_BUDGET, INTERVAL, MAX_BODY, RETRY_INTERVAL, may_contact_cloud};
use crate::agents::entries::{RecordVisitor, SearchKind};
use crate::agents::{AgentPlugin, Message};
use crate::config;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::fs::{self, FileTimes};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Cursor,
    OpenCode,
    Codex,
    Copilot,
    Claude,
}

pub(crate) struct Source {
    kind: Kind,
    endpoint: String,
    authorization: String,
    helper: Option<PathBuf>,
    root: PathBuf,
    cache: PathBuf,
}

#[derive(Clone, Debug)]
struct Row {
    modified: Option<u64>,
    id: String,
    title: String,
    created: Option<u64>,
    revision: Option<String>,
}

#[derive(Deserialize, PartialEq, Eq, Serialize)]
struct Line {
    role: String,
    text: String,
}

#[derive(Deserialize, Serialize)]
struct Snapshot {
    fetched: u64,
    #[serde(default = "messages_coverage")]
    coverage: String,
    modified: Option<u64>,
    title: String,
    created: Option<u64>,
    revision: Option<String>,
    messages: Vec<Line>,
    /// Where an incomplete transcript continues: a long Codex Cloud thread
    /// is fetched over several refreshes, keeping what was read so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
    /// The checkpoint's transcript was read across a change of the thread,
    /// so the finished one is fetched again from the start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    across_change: bool,
}

/// Bridge pages read for one transcript per refresh. Browser runs carry
/// hundreds of non-message items, so a thread can need more; the rest is
/// read on later refreshes.
const CODEX_PAGES_PER_FETCH: usize = 32;

/// A conversation in the Codex desktop app's own catalog
/// (`<CODEX_HOME>/sqlite/codex.db`). The app lists Cloud conversations (made
/// e.g. by the ChatGPT app's browser) under its own thread id and with a
/// title the Cloud service does not return; they are matched to ah's
/// snapshots by creation time.
struct AppThread {
    id: String,
    title: String,
    created: f64,
}

/// The app's Cloud conversations of the signed-in account, read once;
/// empty without the app or a Codex Cloud login.
fn app_threads() -> &'static [AppThread] {
    static THREADS: std::sync::OnceLock<Vec<AppThread>> = std::sync::OnceLock::new();
    THREADS.get_or_init(|| {
        let (Some(base), Ok(login)) = (config::resolve_agent_base("codex"), super::source()) else {
            return Vec::new();
        };
        // Rows are tied to a login by its user id; without it, none are used.
        let Some(user) = login.user.as_deref() else {
            return Vec::new();
        };
        let host = format!("chatgpt:{}:{user}", login.account);
        read_app_threads(&base.join("sqlite/codex.db"), &host).unwrap_or_default()
    })
}

/// Rows of the app catalog `db` for the login's `host`
/// (`chatgpt:<account>:<user>`). The live database is in WAL mode; it is
/// copied, with its log, and the copy is read, so nothing is written next
/// to the app's files.
fn read_app_threads(db: &Path, host: &str) -> Option<Vec<AppThread>> {
    if !db.is_file() {
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    let copy = dir.path().join("codex.db");
    fs::copy(db, &copy).ok()?;
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    if wal.is_file() {
        fs::copy(&wal, dir.path().join("codex.db-wal")).ok()?;
    }
    let conn = rusqlite::Connection::open(&copy).ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT thread_id, display_title, source_created_at FROM local_thread_catalog \
             WHERE source_kind = 'chatgpt' AND missing_candidate = 0 AND host_id = ?1",
        )
        .ok()?;
    let rows = stmt
        .query_map([host], |row| {
            Ok(AppThread {
                id: row.get(0)?,
                title: row.get(1)?,
                created: row.get(2)?,
            })
        })
        .ok()?
        .flatten()
        .collect();
    Some(rows)
}

/// How much later the app records a thread than the Cloud service's
/// whole-second creation time: measured 0.3 to 1.2 seconds, while the app's
/// threads are many seconds apart.
const APP_CREATED_LAG: std::ops::RangeInclusive<f64> = 0.0..=1.5;

fn app_lag_matches(app: f64, cloud: u64) -> bool {
    APP_CREATED_LAG.contains(&(app - cloud as f64))
}

/// The app thread created right after `created`, if exactly one was.
fn app_thread_at(created: Option<u64>) -> Option<&'static AppThread> {
    let created = created?;
    let mut near = app_threads()
        .iter()
        .filter(|t| app_lag_matches(t.created, created));
    let found = near.next()?;
    near.next().is_none().then_some(found)
}

fn codex_snapshot_paths() -> impl Iterator<Item = PathBuf> {
    active(Some("codex"))
        .into_iter()
        .filter(|s| s.kind == Kind::Codex)
        .flat_map(|s| fs::read_dir(&s.cache).into_iter().flatten().flatten())
        .map(|entry| entry.path())
        .filter(|path| source_for_path(path).is_some())
}

/// Creation times of the Codex Cloud snapshots, with how many share each.
fn codex_snapshot_times() -> &'static std::collections::HashMap<u64, usize> {
    static TIMES: std::sync::OnceLock<std::collections::HashMap<u64, usize>> =
        std::sync::OnceLock::new();
    TIMES.get_or_init(|| {
        let mut times = std::collections::HashMap::new();
        for path in codex_snapshot_paths() {
            if let Some(created) = read(&path).and_then(|s| s.created) {
                *times.entry(created).or_insert(0) += 1;
            }
        }
        times
    })
}

/// The app thread of the snapshot created at `created`, when the two match
/// one to one: no other app thread, and no other snapshot, at that time.
fn app_thread_of_snapshot(created: Option<u64>) -> Option<&'static AppThread> {
    if app_threads().is_empty() {
        return None;
    }
    let thread = app_thread_at(created)?;
    let snapshots: usize = codex_snapshot_times()
        .iter()
        .filter(|(t, _)| app_lag_matches(thread.created, **t))
        .map(|(_, n)| n)
        .sum();
    (snapshots == 1).then_some(thread)
}

/// The Codex Cloud snapshot of the app thread `id`, so the id the app shows
/// works with `ah show` and `ah resume`.
pub(crate) fn app_thread_path(id: &str) -> Option<PathBuf> {
    let thread = app_threads().iter().find(|t| t.id == id)?;
    let mut found = codex_snapshot_paths().filter(|path| {
        app_thread_of_snapshot(read(path).and_then(|s| s.created))
            .is_some_and(|t| t.id == thread.id)
    });
    let path = found.next()?;
    found.next().is_none().then_some(path)
}

/// The first line of the first prompt, as the title of a conversation the
/// service left untitled.
fn first_prompt_title(lines: &[Line]) -> Option<String> {
    lines
        .iter()
        .find(|line| line.role == "user")
        .and_then(|line| line.text.lines().find(|l| !l.trim().is_empty()))
        .map(|line| line.trim().chars().take(180).collect())
}

fn open_code_origin(input: &str) -> Option<String> {
    let input = input.strip_suffix('/').unwrap_or(input);
    let (scheme, authority) = input.split_once("://")?;
    if authority.is_empty()
        || !authority
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-:[]".contains(&c))
    {
        return None;
    }
    // Port and host parsing are delegated to the HTTP client's URI type.
    let uri: ureq::http::Uri = input.parse().ok()?;
    let host = uri.host()?;
    if scheme != "https" && !(scheme == "http" && matches!(host, "127.0.0.1" | "[::1]")) {
        return None;
    }
    Some(input.to_owned())
}

// Public output may include cache paths. A private random key prevents a
// path from serving as an offline verifier for a low-entropy server password.
#[cfg(unix)]
fn cache_key(root: &Path) -> Option<[u8; 32]> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    if !super::private_dir(root) {
        return None;
    }
    let path = root.join(".namespace-key");
    let read_key = || {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .ok()?;
        let m = f.metadata().ok()?;
        if !m.is_file()
            || m.len() != 32
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o077 != 0
        {
            return None;
        }
        let mut key = [0u8; 32];
        f.read_exact(&mut key).ok()?;
        Some(key)
    };
    if let Some(key) = read_key() {
        return Some(key);
    }
    if fs::symlink_metadata(&path).is_ok() {
        return None;
    }
    let mut key = [0u8; 32];
    fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut key)
        .ok()?;
    let mut file = tempfile::NamedTempFile::new_in(root).ok()?;
    file.write_all(&key).ok()?;
    match file.persist_noclobber(&path) {
        Ok(_) => Some(key),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => read_key(),
        Err(_) => None,
    }
}

#[cfg(not(unix))]
fn cache_key(_: &Path) -> Option<[u8; 32]> {
    None
}

static CLAUDE_IDENTITY: OnceLock<Value> = OnceLock::new();

/// Only foreground CLI dispatch may obtain the current account. Offline,
/// MCP, SSH children and fzf never start a Claude subprocess or expose a
/// previously-authenticated account's cache.
pub(crate) fn pin_claude_account(agent: Option<&str>) {
    if !may_contact_cloud() || agent.is_some_and(|a| a != "claude") {
        return;
    }
    let Some(entry) = config::cloud_entries().and_then(|entries| entries.get("claude")) else {
        return;
    };
    let Some(helper) = entry
        .helper
        .as_deref()
        .filter(|p| entry.enabled && p.is_absolute())
    else {
        return;
    };
    if !entry.repo.as_ref().is_some_and(|p| p.is_absolute()) {
        return;
    }
    let Ok(response) = run_helper(
        helper,
        serde_json::json!({"identity": true}),
        Instant::now() + std::time::Duration::from_millis(800),
    ) else {
        return;
    };
    if let Some(account) = response.get("identity").filter(|v| {
        v.as_array().is_some_and(|v| {
            v.len() == 2
                && v.iter().all(|part| {
                    part.as_str()
                        .is_some_and(|s| !s.is_empty() && s.len() <= 256)
                })
        })
    }) {
        let _ = CLAUDE_IDENTITY.set(account.clone());
    }
}

fn sources() -> &'static [Source] {
    // The CLI pins its login for one command. MCP is long-lived and may
    // outlive a login or credential change; do not expose these snapshots
    // there until all providers support per-request identity pinning.
    if super::MCP_CACHE.get().is_some() {
        return &[];
    }
    static SOURCES: OnceLock<Vec<Source>> = OnceLock::new();
    SOURCES.get_or_init(|| {
        if !cfg!(unix) {
            return Vec::new();
        }
        let home = crate::agents::common::canonical_home();
        let root = home.join(".ah-provider-cloud");
        // Never distribute the credential namespace key with a cache backup.
        let private = home.join(".ah-provider-private");
        let Some(settings) = config::cloud_entries() else {
            return Vec::new();
        };
        let mut sources = Vec::new();
        for (name, entry) in settings {
            if !entry.enabled {
                continue;
            }
            if name == "claude" {
                let Some(account) = CLAUDE_IDENTITY.get() else {
                    continue;
                };
                let (Some(helper), Some(repo)) = (&entry.helper, &entry.repo) else {
                    continue;
                };
                let Some(key) = cache_key(&private) else {
                    continue;
                };
                let mut mac =
                    Hmac::<Sha256>::new_from_slice(&key).expect("HMAC accepts 32-byte keys");
                mac.update(b"claude\0");
                mac.update(account.to_string().as_bytes());
                let namespace = crate::agents::common::hex(&mac.finalize().into_bytes());
                sources.push(Source {
                    kind: Kind::Claude,
                    endpoint: repo.to_string_lossy().into_owned(),
                    authorization: String::new(),
                    helper: Some(helper.clone()),
                    root: root.clone(),
                    cache: root.join(format!("claude-{namespace}")),
                });
                continue;
            }
            if name == "codex_conversations" {
                let Some(helper) = entry.helper.as_ref().filter(|p| p.is_absolute()) else {
                    continue;
                };
                let Ok(login) = super::source() else { continue };
                let Some(namespace) = login.cache.file_name() else {
                    continue;
                };
                sources.push(Source {
                    kind: Kind::Codex,
                    endpoint: String::new(),
                    authorization: String::new(),
                    helper: Some(helper.clone()),
                    root: root.clone(),
                    cache: root.join(format!("{name}-{}", namespace.to_string_lossy())),
                });
                continue;
            }
            let (kind, default_env) = match name.as_str() {
                "copilot" => (Kind::Copilot, "GH_TOKEN"),
                "cursor" => (Kind::Cursor, "CURSOR_API_KEY"),
                "opencode" => (Kind::OpenCode, "OPENCODE_PASSWORD"),
                _ => continue,
            };
            let Some(secret) =
                std::env::var(entry.credential_env.as_deref().unwrap_or(default_env)).ok()
            else {
                continue;
            };
            if secret.is_empty() || secret.len() > 4096 || !super::safe_header(&secret) {
                continue;
            }
            let (endpoint, username, password) = match kind {
                Kind::Cursor => ("https://api.cursor.com".into(), secret.as_str(), ""),
                Kind::OpenCode => {
                    let Some(endpoint) = entry.endpoint.as_deref().and_then(open_code_origin)
                    else {
                        continue;
                    };
                    let username = entry.username.as_deref().unwrap_or("opencode");
                    if !super::safe_header(username) || username.contains(':') {
                        continue;
                    }
                    (endpoint, username, secret.as_str())
                }
                Kind::Copilot => ("https://api.github.com".into(), secret.as_str(), ""),
                Kind::Codex | Kind::Claude => unreachable!(),
            };
            if kind == Kind::Cursor && username.contains(':') {
                continue;
            }
            let value = format!("{username}:{password}");
            let Some(key) = cache_key(&private) else {
                continue;
            };
            let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("HMAC accepts 32-byte keys");
            mac.update(name.as_bytes());
            mac.update(&[0]);
            mac.update(endpoint.as_bytes());
            mac.update(&[0]);
            mac.update(value.as_bytes());
            let namespace = crate::agents::common::hex(&mac.finalize().into_bytes());
            sources.push(Source {
                kind,
                endpoint,
                authorization: if kind == Kind::Copilot {
                    format!("Bearer {secret}")
                } else {
                    format!("Basic {}", STANDARD.encode(value))
                },
                helper: None,
                root: root.clone(),
                cache: root.join(format!("{name}-{namespace}")),
            });
        }
        sources.sort_by_key(Source::name);
        sources
    })
}

pub(crate) fn active(agent: Option<&str>) -> Vec<&'static Source> {
    sources()
        .iter()
        .filter(|s| {
            agent.is_none_or(|a| a == s.agent())
                && config::active_agents().any(|a| a.id == s.agent())
        })
        .collect()
}

pub(crate) fn source_for_path(path: &Path) -> Option<&'static Source> {
    if path.extension()?.to_str()? != "json" {
        return None;
    }
    let parent = path.parent()?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let stem = path.file_stem()?.to_str()?;
    let sources = sources();
    let source = sources
        .iter()
        .find(|s| parent == s.cache && s.valid_id(stem))
        .or_else(|| {
            // Only resolve aliases when the file name is valid for a configured
            // provider. Ordinary agent history should not incur a realpath call.
            if (parent.is_absolute()
                && !parent
                    .components()
                    .any(|part| part.as_os_str() == ".ah-provider-cloud"))
                || !sources.iter().any(|s| s.valid_id(stem))
            {
                return None;
            }
            let resolved = fs::canonicalize(parent).ok()?;
            sources
                .iter()
                .find(|s| resolved == s.cache && s.valid_id(stem))
        })?;
    if !source.private_cache() || open_snapshot(path).is_none() {
        return None;
    }
    Some(source)
}

pub(crate) fn cache_patterns() -> impl Iterator<Item = String> {
    active(None)
        .into_iter()
        .filter(|s| s.private_cache())
        .map(|s| {
            format!(
                "{}/*.json",
                glob::Pattern::escape(&s.cache.to_string_lossy())
            )
        })
}

impl Source {
    pub(crate) fn name(&self) -> &'static str {
        match self.kind {
            Kind::Cursor => "Cursor Cloud",
            Kind::OpenCode => "OpenCode server",
            Kind::Codex => "Codex Cloud conversations",
            Kind::Copilot => "Copilot Cloud tasks",
            Kind::Claude => "Claude Code Cloud",
        }
    }
    pub(crate) fn agent(&self) -> &'static str {
        match self.kind {
            Kind::Cursor => "cursor",
            Kind::OpenCode => "opencode",
            Kind::Codex => "codex",
            Kind::Copilot => "copilot",
            Kind::Claude => "claude",
        }
    }
    fn private_cache(&self) -> bool {
        super::private_dir(&self.root) && super::private_dir(&self.cache)
    }
    fn valid_id(&self, id: &str) -> bool {
        if matches!(self.kind, Kind::Codex | Kind::Copilot) {
            return is_uuid(id);
        }
        if self.kind == Kind::Claude {
            return claude_cloud_id(id);
        }
        let prefix = match self.kind {
            Kind::Cursor if id.starts_with("bc-") => "bc-",
            Kind::Cursor => "bc_",
            Kind::OpenCode => "ses_",
            Kind::Codex | Kind::Copilot | Kind::Claude => unreachable!(),
        };
        id.strip_prefix(prefix).is_some_and(|x| {
            !x.is_empty()
                && x.len() <= 128
                && x.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
        })
    }
    fn path(&self, id: &str) -> PathBuf {
        self.cache.join(format!("{id}.json"))
    }
    /// An unfinished transcript fetch, kept apart from the published
    /// snapshot so a complete one is not replaced by a part of it.
    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.cache.join(format!(".{id}.partial"))
    }
    fn drop_checkpoint(&self, id: &str) {
        let _ = fs::remove_file(self.checkpoint_path(id));
    }
    /// Store a fetched transcript. A finished one is published and ends any
    /// checkpoint. An unfinished one becomes the checkpoint; it is published
    /// only while nothing more complete is, so it can be found meanwhile.
    fn publish(&self, id: &str, snapshot: Snapshot) -> Result<PathBuf, &'static str> {
        if snapshot.cursor.is_none() {
            let published = self.store(id, snapshot)?;
            self.drop_checkpoint(id);
            return Ok(published);
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.cache).map_err(|_| "cannot save progress")?;
        serde_json::to_writer(&mut temp, &snapshot).map_err(|_| "cannot save progress")?;
        temp.persist(self.checkpoint_path(id))
            .map_err(|_| "cannot save progress")?;
        let path = self.path(id);
        let complete =
            read(&path).is_some_and(|s| s.coverage == "messages" && !s.messages.is_empty());
        if complete || (snapshot.messages.is_empty() && path.exists()) {
            return Ok(path);
        }
        let mut shown = snapshot;
        shown.cursor = None;
        shown.across_change = false;
        // Fetched again until done (see the checkpoint check in `refresh`).
        shown.revision = None;
        self.store(id, shown)
    }
    fn http(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .max_redirects(0)
            .proxy(None)
            .https_only(self.endpoint.starts_with("https://"))
            .build()
            .into()
    }
    fn get(
        &self,
        http: &ureq::Agent,
        route: &str,
        deadline: Instant,
    ) -> Result<Value, &'static str> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or("deadline reached")?;
        let mut response = http
            .get(format!("{}{route}", self.endpoint))
            .header("Authorization", &self.authorization)
            .header("Accept", "application/json")
            .header("User-Agent", "ah-cli")
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()
            .map_err(|e| match e {
                ureq::Error::StatusCode(401 | 403) => "login rejected",
                ureq::Error::StatusCode(404) => "session not found or not accessible",
                _ => "request failed or timed out",
            })?;
        if response.status() != 200 {
            return Err("unexpected HTTP response");
        }
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_BODY + 1)
            .read_to_end(&mut body)
            .map_err(|_| "could not read response")?;
        if body.len() as u64 > MAX_BODY {
            return Err("response exceeds size limit");
        }
        serde_json::from_slice(&body).map_err(|_| "unsupported response")
    }
    fn row(&self, row: &Value) -> Result<Row, &'static str> {
        let id = row["id"]
            .as_str()
            .filter(|id| self.valid_id(id))
            .ok_or("invalid session ID")?;
        let (title, created, revision) = match self.kind {
            Kind::Claude => return Err("Claude Cloud has no list route"),
            Kind::Codex => (
                row["name"].as_str(),
                row["created"].as_u64(),
                row["updated"].as_u64().map(|n| n.to_string()),
            ),
            Kind::Copilot => (
                row["name"].as_str(),
                rfc3339(row["created_at"].as_str()),
                row["updated_at"].as_str().map(str::to_owned),
            ),
            Kind::Cursor => (
                row["name"].as_str(),
                row["createdAt"]
                    .as_str()
                    .and_then(crate::datetime::parse_rfc3339)
                    .and_then(|t| u64::try_from(crate::datetime::unix_seconds(t)).ok()),
                row["updatedAt"].as_str().map(str::to_owned),
            ),
            Kind::OpenCode => (
                row["title"].as_str(),
                row["time"]["created"].as_u64().map(|ms| ms / 1000),
                row["time"]["updated"].as_u64().map(|n| n.to_string()),
            ),
        };
        let modified = match self.kind {
            Kind::Claude => None,
            Kind::Codex => row["updated"].as_u64(),
            Kind::Copilot => rfc3339(row["updated_at"].as_str()),
            Kind::Cursor => row["updatedAt"]
                .as_str()
                .and_then(crate::datetime::parse_rfc3339)
                .and_then(|t| u64::try_from(crate::datetime::unix_seconds(t)).ok()),
            Kind::OpenCode => row["time"]["updated"].as_u64().map(|ms| ms / 1000),
        };
        Ok(Row {
            modified: modified.or(created),
            id: id.to_owned(),
            title: title.filter(|t| !t.trim().is_empty()).unwrap_or(id).into(),
            created,
            revision,
        })
    }
    fn detail(
        &self,
        http: &ureq::Agent,
        id: &str,
        hint: Option<Row>,
        resume: Option<Snapshot>,
        deadline: Instant,
    ) -> Result<Snapshot, &'static str> {
        match self.kind {
            Kind::Claude => {
                let response = run_helper(
                    self.helper
                        .as_deref()
                        .ok_or("Claude Cloud helper not configured")?,
                    serde_json::json!({
                        "id": id, "repo": self.endpoint, "account": CLAUDE_IDENTITY.get()
                    }),
                    deadline,
                )?;
                if response["id"].as_str() != Some(id) {
                    return Err("session ID mismatch");
                }
                let lines: Vec<Line> = serde_json::from_value(response["messages"].clone())
                    .map_err(|_| "invalid Claude Cloud export")?;
                let title = lines
                    .iter()
                    .find(|l| l.role == "user")
                    .and_then(|l| l.text.lines().next())
                    .filter(|l| !l.is_empty())
                    .ok_or("no user messages in Claude Cloud export")?
                    .chars()
                    .take(180)
                    .collect();
                let mut data = snapshot(
                    Row {
                        id: id.into(),
                        title,
                        modified: None,
                        created: None,
                        revision: None,
                    },
                    lines,
                );
                data.coverage = "teleport-export".into();
                Ok(data)
            }
            Kind::Codex => {
                let mut row = hint.unwrap_or_else(|| Row {
                    id: id.into(),
                    title: id.into(),
                    modified: None,
                    created: None,
                    revision: None,
                });
                // Continue an incomplete transcript where the last refresh
                // stopped. A thread still being written to keeps changing, so
                // the fetch goes on across changes; a transcript put together
                // across a change is checked once more from the start.
                let start_revision = row.revision.clone();
                let mut across_change = false;
                let (mut lines, mut cursor, mut first_created, mut last_updated) = match resume {
                    Some(saved) if saved.cursor.is_some() => {
                        across_change = saved.across_change || saved.revision != start_revision;
                        (saved.messages, saved.cursor, saved.created, saved.modified)
                    }
                    _ => (Vec::new(), None, None, None),
                };
                let resumed = cursor.is_some();
                // A resumed fetch that hits a broken page cannot go on from
                // its saved position: start over next time.
                let fail = |e: &'static str| {
                    if resumed {
                        self.drop_checkpoint(id);
                    }
                    e
                };
                let mut length: usize = lines.iter().map(|l| l.text.len()).sum();
                let mut seen: std::collections::HashSet<String> = cursor.iter().cloned().collect();
                let mut pages = 0;
                let complete = loop {
                    if pages == CODEX_PAGES_PER_FETCH {
                        break false;
                    }
                    let page = match self.run_bridge(Some(id), cursor.as_deref(), deadline) {
                        Ok(page) => page,
                        // Out of time: keep what was read for the next refresh.
                        Err(_) if pages > 0 => break false,
                        // Out of time before the first page: try again later.
                        Err(e @ ("deadline reached" | "helper timed out")) => return Err(e),
                        // The saved position no longer works (an expired
                        // cursor, a deleted thread): start over next time.
                        Err(e) => return Err(fail(e)),
                    };
                    pages += 1;
                    if page["id"].as_str() != Some(id) {
                        return Err(fail("session ID mismatch"));
                    }
                    if let Some(t) = page["created"].as_u64() {
                        first_created = Some(first_created.map_or(t, |old| old.min(t)));
                    }
                    if let Some(t) = page["updated"].as_u64() {
                        last_updated = Some(last_updated.map_or(t, |old| old.max(t)));
                    }
                    let messages = page["messages"]
                        .as_array()
                        .ok_or_else(|| fail("unsupported transcript"))?;
                    for msg in messages {
                        length += msg["text"]
                            .as_str()
                            .ok_or_else(|| fail("invalid transcript item"))?
                            .len();
                        if length > MAX_BODY as usize {
                            return Err(fail("transcript exceeds size limit"));
                        }
                        lines.push(
                            serde_json::from_value(msg.clone())
                                .map_err(|_| fail("invalid transcript item"))?,
                        );
                    }
                    let Some(next) = page["nextCursor"].as_str() else {
                        break true;
                    };
                    // The saved cursor counts as seen: a page pointing back
                    // to it loops.
                    if !seen.insert(next.to_owned()) {
                        return Err(fail("repeated pagination cursor"));
                    }
                    cursor = Some(next.to_owned());
                };
                row.created = row.created.or(first_created);
                row.modified = row.modified.or(last_updated).or(row.created);
                // An untitled thread keeps its empty title (or id): the title
                // shown is chosen when listing (see `resolve_title`).
                if complete {
                    if across_change {
                        // Not the revision of any single moment: fetch again.
                        row.revision = None;
                    }
                    return Ok(snapshot(row, lines));
                }
                // The checkpoint keeps the revision the fetch started from.
                row.revision = start_revision;
                let mut data = snapshot(row, lines);
                data.coverage = "messages-partial".into();
                data.cursor = cursor;
                data.across_change = across_change;
                Ok(data)
            }
            Kind::Copilot => {
                let detail = self.get(http, &format!("/agents/tasks/{id}"), deadline)?;
                let row = self.row(&detail)?;
                if row.id != id {
                    return Err("session ID mismatch");
                }
                let sessions = detail["sessions"]
                    .as_array()
                    .ok_or("unsupported task sessions")?;
                let mut messages = Vec::with_capacity(sessions.len());
                for s in sessions {
                    if s["task_id"].as_str() != Some(id) {
                        return Err("task ID mismatch");
                    }
                    if let Some(prompt) = s["prompt"].as_str().filter(|p| !p.is_empty()) {
                        messages.push(Line {
                            role: "user".into(),
                            text: prompt.into(),
                        });
                    }
                }
                let mut snapshot = snapshot(row, messages);
                if !snapshot.messages.is_empty() {
                    snapshot.coverage = "prompt-only".into();
                }
                Ok(snapshot)
            }
            Kind::Cursor => {
                let row = match hint {
                    Some(row) => row,
                    None => self.row(&self.get(http, &format!("/v0/agents/{id}"), deadline)?)?,
                };
                let detail = self.get(http, &format!("/v0/agents/{id}/conversation"), deadline)?;
                if detail["id"].as_str() != Some(id) || row.id != id {
                    return Err("session ID mismatch");
                }
                Ok(snapshot(row, cursor_messages(&detail)?))
            }
            Kind::OpenCode => {
                let raw = self.get(
                    http,
                    &format!("/api/experimental/session/{id}/export"),
                    deadline,
                )?;
                let data = &raw["data"];
                let row = self.row(&data["info"])?;
                if row.id != id {
                    return Err("session ID mismatch");
                }
                Ok(snapshot(row, opencode_messages(data)?))
            }
        }
    }
    pub(super) fn due(&self, force: bool) -> bool {
        self.kind != Kind::Claude
            && (force
                || !self.private_cache()
                || !(super::recent_stamp(&self.cache.join(".last-success"), INTERVAL)
                    || super::recent_stamp(&self.cache.join(".last-attempt"), RETRY_INTERVAL)))
    }
    pub(super) fn refresh(&self, force: bool, deadline: Instant) -> Result<(), &'static str> {
        let Some(_lock) = super::lock_dirs(&self.root, &self.cache)? else {
            return if force { Err("cache is busy") } else { Ok(()) };
        };
        if !force
            && (super::recent_stamp(&self.cache.join(".last-success"), INTERVAL)
                || super::recent_stamp(&self.cache.join(".last-attempt"), RETRY_INTERVAL))
        {
            return Ok(());
        }
        super::write_stamp(&self.cache, ".last-attempt")?;
        let http = self.http();
        let mut cursor = String::new();
        let mut previous = self.saved_cursor();
        let mut seen_cursors = std::collections::HashSet::new();
        let mut seen = std::collections::HashSet::new();
        let mut limited = false;
        let mut error = None;
        let mut invalid_rows = false;
        let mut revalidation_used = false;
        // Always scan the current head. If a previous scan reached older
        // pages, resume that backfill after processing the head.
        let pages = 4;
        for page_no in 0..pages {
            let (page, key) = match self.kind {
                Kind::Claude => return Err("Claude Cloud has no list route; use the exact URL"),
                Kind::Cursor => {
                    let query = if cursor.is_empty() {
                        String::new()
                    } else {
                        format!("&cursor={}", encode_query(&cursor))
                    };
                    (
                        self.get(&http, &format!("/v0/agents?limit=20{query}"), deadline),
                        "agents",
                    )
                }
                Kind::OpenCode => (self.get(&http, "/api/session", deadline), "data"),
                Kind::Copilot => (
                    self.get(&http, "/agents/tasks?per_page=20", deadline),
                    "tasks",
                ),
                Kind::Codex => (
                    self.run_bridge(
                        None,
                        (!cursor.is_empty()).then_some(cursor.as_str()),
                        deadline,
                    ),
                    "data",
                ),
            };
            let page = match page {
                Ok(page) => page,
                Err(e) => {
                    // The old cursor may have expired. Restart backfill at the
                    // head on a later request instead of retrying it forever.
                    if !cursor.is_empty() && Instant::now() < deadline {
                        self.save_cursor(None)?;
                    }
                    return Err(e);
                }
            };
            let rows = page[key].as_array().ok_or("unsupported session list")?;
            let limit = if self.kind == Kind::OpenCode {
                1000
            } else {
                20
            };
            limited |= rows.len() > limit;
            let mut changed = Vec::new();
            let mut unchanged_oldest: Option<(u64, Row)> = None;
            for entry in rows.iter().take(limit) {
                let Ok(row) = self.row(entry) else {
                    invalid_rows = true;
                    continue;
                };
                if !seen.insert(row.id.clone()) {
                    continue;
                }
                let cached = read(&self.path(&row.id));
                // The title/ID may be useful even when the budget is too
                // small for an older or larger transcript.
                if cached.is_none() {
                    let mut meta = snapshot(row.clone(), Vec::new());
                    meta.fetched = 0;
                    self.store(&row.id, meta)?;
                }
                // Revalidate periodically even if upstream time has not
                // changed; --refresh bypasses the refresh TTL, not this
                // per-record debounce.
                let unfinished = self.checkpoint_path(&row.id).exists()
                    || cached
                        .as_ref()
                        .is_some_and(|c| c.coverage == "messages-partial");
                match cached {
                    Some(current)
                        if current.fetched != 0
                            && !unfinished
                            && (row.revision.is_none() || current.revision == row.revision) =>
                    {
                        if !revalidation_used
                            && unix_now().saturating_sub(current.fetched) >= 900
                            && unchanged_oldest
                                .as_ref()
                                .is_none_or(|(t, _)| current.fetched < *t)
                        {
                            unchanged_oldest = Some((current.fetched, row));
                        }
                    }
                    _ => changed.push(row),
                }
            }
            if let Some((_, row)) = unchanged_oldest {
                changed.push(row);
                revalidation_used = true;
            }
            // Threads still being fetched in parts go last, so a long one
            // does not hold back new conversations.
            // So does the recheck of a complete one fetched across a change.
            changed.sort_by_key(|row| {
                self.checkpoint_path(&row.id).exists()
                    || read(&self.path(&row.id)).is_some_and(|s| {
                        s.coverage == "messages" && s.revision.is_none() && !s.messages.is_empty()
                    })
            });
            for row in changed {
                if Instant::now() >= deadline {
                    limited = true;
                    break;
                }
                let id = row.id.clone();
                let retry = format!(".{id}.retry");
                let cooldown = std::time::Duration::from_secs(180);
                if !force && super::recent_stamp(&self.cache.join(&retry), cooldown) {
                    continue;
                }
                let resume = read(&self.checkpoint_path(&id));
                if let Err(e) = self
                    .detail(&http, &id, Some(row), resume, deadline)
                    .and_then(|s| self.publish(&id, s).map(drop))
                {
                    error.get_or_insert(e);
                    super::write_stamp(&self.cache, &retry)?;
                }
            }
            let next = page["nextCursor"].as_str().filter(|s| !s.is_empty());
            if matches!(self.kind, Kind::OpenCode | Kind::Copilot) {
                limited |= self.kind == Kind::Copilot && rows.len() == 20;
                break;
            }
            let next = if page_no == 0 && next.is_some() {
                previous.take().or_else(|| next.map(str::to_owned))
            } else {
                next.map(str::to_owned)
            };
            if let Some(ref token) = next
                && (token.len() > 1024
                    || token.chars().any(char::is_control)
                    || !seen_cursors.insert(token.clone()))
            {
                self.save_cursor(None)?;
                return Err("invalid or repeated pagination cursor");
            }
            self.save_cursor(next.as_deref())?;
            let Some(next) = next else {
                break;
            };
            cursor = next;
            if page_no + 1 == pages {
                limited = true;
            }
        }
        // An isolated transcript error does not undo a valid discovery
        // pass, or make every automatic invocation retry the same record.
        super::write_stamp(&self.cache, ".last-success")?;
        if force {
            if let Some(error) = error {
                return Err(error);
            }
            if limited || invalid_rows {
                return Err("refresh reached a page limit or skipped unsupported records");
            }
        }
        Ok(())
    }
    fn saved_cursor(&self) -> Option<String> {
        let cursor = String::from_utf8(bytes(&self.cache.join(".next-page"))?).ok()?;
        (!cursor.is_empty() && cursor.len() <= 1024 && !cursor.chars().any(char::is_control))
            .then_some(cursor)
    }

    fn save_cursor(&self, cursor: Option<&str>) -> Result<(), &'static str> {
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.cache).map_err(|_| "cannot save pagination")?;
        temp.write_all(cursor.unwrap_or_default().as_bytes())
            .map_err(|_| "cannot save pagination")?;
        temp.persist(self.cache.join(".next-page"))
            .map_err(|_| "cannot publish pagination")?;
        Ok(())
    }

    fn run_bridge(
        &self,
        id: Option<&str>,
        cursor: Option<&str>,
        deadline: Instant,
    ) -> Result<Value, &'static str> {
        let login = super::source()?;
        let helper = self
            .helper
            .as_ref()
            .ok_or("Codex Cloud helper not configured")?;
        run_helper(
            helper,
            serde_json::json!({
                "owner": login.owner, "account": login.account, "id": id, "cursor": cursor
            }),
            deadline,
        )
    }

    fn store(&self, id: &str, mut snapshot: Snapshot) -> Result<PathBuf, &'static str> {
        let path = self.path(id);
        if self.kind == Kind::Claude && snapshot.modified.is_none() {
            // The terminal export has no trustworthy upstream timestamp.
            // Keep the last observed change time if revalidation finds the same text.
            snapshot.modified = Some(
                read(&path)
                    .filter(|saved| saved.messages == snapshot.messages)
                    .and_then(|saved| saved.modified)
                    .unwrap_or(snapshot.fetched),
            );
        }
        if snapshot.messages.is_empty() && read(&path).is_some_and(|s| !s.messages.is_empty()) {
            return Err("empty response; previous history preserved");
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.cache).map_err(|_| "cannot create snapshot")?;
        if serde_json::to_writer(&mut temp, &snapshot).is_err()
            || temp.flush().is_err()
            || temp
                .as_file()
                .metadata()
                .map_or(true, |m| m.len() > MAX_BODY)
        {
            return Err("could not write snapshot");
        }
        temp.as_file()
            .set_times(
                FileTimes::new().set_modified(
                    snapshot
                        .modified
                        .and_then(|n| super::epoch(n as f64))
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                ),
            )
            .map_err(|_| "could not set snapshot timestamp")?;
        temp.persist(&path)
            .map_err(|_| "could not publish snapshot")?;
        Ok(path)
    }
    fn url(&self, id: &str) -> String {
        match self.kind {
            Kind::Claude => format!("https://claude.ai/code/{id}"),
            Kind::Cursor if id.starts_with("bc-") => format!("https://cursor.com/agents/{id}"),
            Kind::Cursor => format!("https://cursor.com/agents?id={id}"),
            Kind::OpenCode => format!(
                "{}/server/{}/session/{id}",
                self.endpoint,
                URL_SAFE_NO_PAD.encode(&self.endpoint)
            ),
            Kind::Copilot => String::new(),
            Kind::Codex => format!("https://chatgpt.com/local/{id}"),
        }
    }
}

#[cfg(unix)]
fn run_helper(helper: &Path, request: Value, deadline: Instant) -> Result<Value, &'static str> {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    use std::process::{Command, Stdio};
    if Instant::now() >= deadline {
        return Err("deadline reached");
    }
    let request = serde_json::to_vec(&request).map_err(|_| "invalid Cloud request")?;
    if request.len() > 2048 {
        return Err("oversized Cloud request");
    }
    let mut child = Command::new(helper)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "could not start Cloud helper")?;
    let result = (|| {
        child
            .stdin
            .take()
            .ok_or("helper input unavailable")?
            .write_all(&request)
            .map_err(|_| "could not write helper request")?;
        let mut stdout = child.stdout.take().ok_or("helper output unavailable")?;
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err("could not set helper deadline");
        }
        let mut data = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if Instant::now() >= deadline {
                return Err("helper timed out");
            }
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if data.len() + n > MAX_BODY as usize {
                        return Err("helper output too large");
                    }
                    data.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => return Err("helper output unavailable"),
            }
        }
        loop {
            match child.try_wait().map_err(|_| "could not wait for helper")? {
                Some(status) if status.success() => break,
                Some(_) => return Err("helper failed; check login, setup and helper version"),
                None if Instant::now() >= deadline => return Err("helper timed out"),
                None => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        serde_json::from_slice(&data).map_err(|_| "unsupported helper response")
    })();
    if result.is_err() {
        // Allow the teleport helper to stop its own terminal and remove
        // its temporary worktree before escalating.
        let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
        let limit = Instant::now() + std::time::Duration::from_millis(800);
        while Instant::now() < limit && child.try_wait().is_ok_and(|s| s.is_none()) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if child.try_wait().is_ok_and(|s| s.is_none()) {
            let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
        }
    }
    result
}

#[cfg(not(unix))]
fn run_helper(_: &Path, _: Value, _: Instant) -> Result<Value, &'static str> {
    Err("Cloud helper requires Unix")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn encode_query(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn rfc3339(v: Option<&str>) -> Option<u64> {
    crate::datetime::parse_rfc3339(v?)
        .and_then(|t| u64::try_from(crate::datetime::unix_seconds(t)).ok())
}

fn messages_coverage() -> String {
    "messages".into()
}

fn snapshot(row: Row, messages: Vec<Line>) -> Snapshot {
    let coverage = if messages.is_empty() {
        "metadata".into()
    } else {
        messages_coverage()
    };
    Snapshot {
        coverage,
        fetched: unix_now(),
        modified: row.modified,
        title: row.title,
        created: row.created,
        revision: row.revision,
        messages,
        cursor: None,
        across_change: false,
    }
}

fn cursor_messages(detail: &Value) -> Result<Vec<Line>, &'static str> {
    detail["messages"]
        .as_array()
        .ok_or("unsupported transcript")?
        .iter()
        .map(|msg| {
            let role = match msg["type"].as_str() {
                Some("user_message") => "user",
                Some("assistant_message") => "assistant",
                _ => return Err("unsupported transcript role"),
            };
            let text = msg["text"].as_str().ok_or("unsupported transcript text")?;
            Ok(Line {
                role: role.into(),
                text: text.into(),
            })
        })
        .collect()
}

fn opencode_messages(detail: &Value) -> Result<Vec<Line>, &'static str> {
    let mut lines = Vec::new();
    for msg in detail["messages"]
        .as_array()
        .ok_or("unsupported transcript")?
    {
        let (role, text) = match msg["type"].as_str() {
            Some("user") => ("user", msg["text"].as_str().unwrap_or_default().to_owned()),
            Some("assistant") => (
                "assistant",
                msg["content"]
                    .as_array()
                    .ok_or("unsupported assistant message")?
                    .iter()
                    .filter(|p| p["type"] == "text")
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => continue,
        };
        if !text.is_empty() {
            lines.push(Line {
                role: role.into(),
                text,
            });
        }
    }
    Ok(lines)
}

#[cfg(unix)]
fn open_snapshot(path: &Path) -> Option<fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let m = file.metadata().ok()?;
    if !m.is_file()
        || m.len() > MAX_BODY
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
    {
        return None;
    }
    Some(file)
}

#[cfg(not(unix))]
fn open_snapshot(_: &Path) -> Option<fs::File> {
    None
}

fn bytes(path: &Path) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    open_snapshot(path)?
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .ok()?;
    (body.len() as u64 <= MAX_BODY).then_some(body)
}

fn read(path: &Path) -> Option<Snapshot> {
    serde_json::from_slice(&bytes(path)?).ok()
}

fn routed(value: &str, agent: Option<&str>) -> Option<(&'static Source, String)> {
    let candidates: Vec<_> = active(agent)
        .into_iter()
        .filter_map(|s| {
            let id = match s.kind {
                Kind::Claude => value.strip_prefix("https://claude.ai/code/")?,
                Kind::Codex => value.strip_prefix("https://chatgpt.com/local/")?,
                Kind::Cursor => value
                    .strip_prefix("https://cursor.com/agents?id=")
                    .or_else(|| value.strip_prefix("https://cursor.com/agents/"))
                    .unwrap_or(value),
                Kind::Copilot if agent == Some("copilot") => value,
                Kind::Copilot => return None,
                Kind::OpenCode => {
                    let prefix = format!(
                        "{}/server/{}/session/",
                        s.endpoint,
                        URL_SAFE_NO_PAD.encode(&s.endpoint)
                    );
                    value.strip_prefix(&prefix).unwrap_or(value)
                }
            };
            s.valid_id(id).then(|| (s, id.to_owned()))
        })
        .collect();
    if candidates.len() == 1 {
        candidates.into_iter().next()
    } else {
        None
    }
}

pub(crate) fn is_url_ref(value: &str, agent: Option<&str>) -> bool {
    value.contains("://") && routed(value, agent).is_some()
}

pub(crate) fn fetch(
    value: &str,
    offline: bool,
    agent: Option<&str>,
) -> Result<Option<PathBuf>, String> {
    let Some((source, id)) = routed(value, agent) else {
        return Ok(None);
    };
    fetch_source(
        source,
        &id,
        offline,
        source.kind == Kind::Claude && value.contains("://"),
    )
}

pub(crate) fn hydrate(path: &Path, offline: bool) -> Result<(), String> {
    if let Some(source) = source_for_path(path)
        && let Some(id) = path.file_stem().and_then(|s| s.to_str())
    {
        fetch_source(source, id, offline, false)?;
    }
    Ok(())
}

fn fetch_source(
    source: &Source,
    id: &str,
    offline: bool,
    revalidate_claude: bool,
) -> Result<Option<PathBuf>, String> {
    let path = source.path(id);
    let read_cached = || read(&path).filter(|_| source_for_path(&path).is_some());
    let fresh = |s: &Snapshot| {
        s.fetched != 0
            && (!revalidate_claude || unix_now().saturating_sub(s.fetched) < INTERVAL.as_secs())
    };
    let cached = read_cached();
    if cached.as_ref().is_some_and(fresh) || offline || !may_contact_cloud() {
        return Ok(cached.map(|_| path));
    }
    let work = || {
        let deadline = Instant::now()
            + if source.kind == Kind::Claude {
                std::time::Duration::from_secs(24)
            } else {
                FOREGROUND_BUDGET
            };
        let Some(_lock) = super::lock_dirs(&source.root, &source.cache)? else {
            return if cached.is_some() {
                Ok(Some(path.clone()))
            } else {
                Err("cache is busy")
            };
        };
        let cached = read_cached();
        if cached.as_ref().is_some_and(fresh) {
            return Ok(Some(path.clone()));
        }
        let http = source.http();
        let hint = cached
            .as_ref()
            .filter(|_| source.kind == Kind::Codex)
            .map(|s| Row {
                id: id.to_owned(),
                title: s.title.clone(),
                created: s.created,
                modified: s.modified,
                revision: s.revision.clone(),
            });
        let resume = read(&source.checkpoint_path(id));
        match source
            .detail(&http, id, hint, resume, deadline)
            .and_then(|snapshot| source.publish(id, snapshot))
        {
            Ok(published) => Ok(Some(published)),
            Err(err) if cached.is_some() && source_for_path(&path).is_some() => {
                eprintln!(
                    "Cloud history not refreshed: {}: {}",
                    source.name(),
                    crate::output::sanitize_multiline(err)
                );
                Ok(Some(path.clone()))
            }
            Err(err) => Err(err),
        }
    };
    work().map_err(|e| format!("{}: {e}", source.name()))
}

pub(crate) fn url_for_path(path: &Path) -> Option<String> {
    let source = source_for_path(path)?;
    Some(source.url(path.file_stem()?.to_str()?))
}

pub(crate) fn coverage(path: &Path) -> String {
    read(path).map_or_else(|| "metadata".into(), |s| s.coverage)
}

fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn claude_cloud_id(id: &str) -> bool {
    id.strip_prefix("session_")
        .or_else(|| id.strip_prefix("cse_"))
        .is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix.len() <= 128
                && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

pub(crate) fn looks_like_claude_ref(value: &str, home: &Path) -> bool {
    let value = crate::output::strip_quotes(value);
    if claude_cloud_id(value) || value.starts_with("https://claude.ai/code/") {
        return true;
    }
    // Resolve the raw path first, including a symlink or relative alias.
    // Fall back to an escaped TSV path only if the raw value cannot resolve.
    fs::canonicalize(value)
        .or_else(|_| fs::canonicalize(Path::new(&*crate::output::unescape_tsv(value))))
        .is_ok_and(|path| {
            path.strip_prefix(home.join(".ah-provider-cloud"))
                .ok()
                .and_then(|p| p.components().next())
                .and_then(|part| part.as_os_str().to_str())
                .is_some_and(|name| name.starts_with("claude-"))
        })
}

pub(crate) fn configured_claude_url(value: &str, agent: Option<&str>) -> bool {
    agent.is_none_or(|a| a == "claude")
        && config::active_agents().any(|a| a.id == "claude")
        && config::cloud_entries()
            .and_then(|e| e.get("claude"))
            .is_some_and(|e| e.enabled)
        && value
            .strip_prefix("https://claude.ai/code/")
            .is_some_and(claude_cloud_id)
}

pub(crate) fn explicit_resume(value: &str, agent: Option<&str>) -> Option<Vec<String>> {
    if value.contains("://")
        && let Some((source, id)) = routed(value, agent)
    {
        let url = source.url(&id);
        if !url.is_empty() {
            return Some(browser_args(url));
        }
    }
    let entries = config::cloud_entries()?;
    if agent.is_none_or(|a| a == "claude")
        && config::active_agents().any(|a| a.id == "claude")
        && entries.get("claude").is_some_and(|e| e.enabled)
    {
        let id = value.strip_prefix("https://claude.ai/code/")?;
        if claude_cloud_id(id) {
            return Some(browser_args(format!("https://claude.ai/code/{id}")));
        }
    }
    None
}

pub(crate) fn browser_args(url: String) -> Vec<String> {
    vec![
        if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        }
        .into(),
        url,
    ]
}

pub(crate) struct SnapshotPlugin;
pub(crate) static PLUGIN: SnapshotPlugin = SnapshotPlugin;

impl AgentPlugin for SnapshotPlugin {
    fn id(&self) -> &'static str {
        "cloud-snapshot"
    }
    fn description(&self) -> &'static str {
        "Read-only cloud conversation cache"
    }
    fn glob_patterns(&self) -> &'static [&'static str] {
        &[]
    }
    fn path_markers(&self) -> &'static [&'static str] {
        &[]
    }
    fn session_bytes(&self, path: &Path) -> Option<crate::agents::common::SessionBytes> {
        Some(crate::agents::common::SessionBytes::Owned(bytes(path)?))
    }
    fn raw_content(&self, path: &Path) -> Option<String> {
        let data = read(path)?;
        if !data.messages.is_empty() {
            return String::from_utf8(bytes(path)?).ok();
        }
        Some(data.title)
    }
    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        if let Some(data) = read(path) {
            messages(data, visit);
        }
    }
    fn iter_search_records(&self, path: &Path, visit: &mut RecordVisitor<'_>) {
        self.iter_messages(path, &mut |message| {
            visit.record(|emit| emit(SearchKind::from_role(message.role), &message.text))
        });
    }
    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        bytes: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        if let Ok(data) = serde_json::from_slice(bytes) {
            messages(data, visit);
        }
    }
    fn session_id(&self, path: &Path, _home: &Path) -> Option<String> {
        Some(path.file_stem()?.to_str()?.to_owned())
    }
    fn session_created(&self, path: &Path) -> Option<SystemTime> {
        super::epoch(read(path)?.created? as f64)
    }
    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        let snapshot = read(path)?;
        // Some services leave a conversation untitled (an empty title, or
        // just its id); show its first prompt instead.
        let title = if snapshot.title.trim().is_empty()
            || path
                .file_stem()
                .is_some_and(|id| id == snapshot.title.as_str())
        {
            // The Codex app may know the title the service left out.
            source_for_path(path)
                .filter(|s| s.kind == Kind::Codex)
                .and_then(|_| app_thread_of_snapshot(snapshot.created))
                .map(|t| t.title.trim().to_owned())
                .filter(|t| !t.is_empty())
                .or_else(|| first_prompt_title(&snapshot.messages))
                .unwrap_or(snapshot.title)
        } else {
            snapshot.title
        };
        Some(format!("[cloud:{}] {}", snapshot.coverage, title))
    }
    fn resume_args(&self, path: &Path, _home: &Path) -> Option<Vec<String>> {
        let url = url_for_path(path)?;
        (!url.is_empty()).then(|| browser_args(url))
    }
}

fn messages(data: Snapshot, visit: &mut dyn FnMut(Message) -> bool) {
    for line in data.messages {
        let m = match line.role.as_str() {
            "user" => Message::user(line.text),
            "assistant" => Message::assistant(line.text),
            _ => continue,
        };
        if !visit(m) {
            break;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::time::Duration;

    fn fixture(
        kind: Kind,
        replies: Vec<(&'static str, u16, Value)>,
        verify: impl FnOnce(Source, Instant),
    ) {
        let home = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let root = home.path().join("root");
        let source = Source {
            kind,
            endpoint,
            authorization: "Basic dGVzdDo=".to_owned(),
            helper: None,
            cache: root.join("private"),
            root,
        };
        std::thread::scope(|scope| {
            let server = scope.spawn(move || {
                for (expect, status, reply) in replies {
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(Instant::now() < deadline, "missing request for {expect}");
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(e) => panic!("{e}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut lines = BufReader::new(&mut stream);
                    let mut line = String::new();
                    lines.read_line(&mut line).unwrap();
                    assert!(line.starts_with(&format!("GET {expect} ")), "{line}");
                    let mut auth = false;
                    loop {
                        line.clear();
                        let size = lines.read_line(&mut line).unwrap();
                        if size == 0 || line == "\r\n" {
                            break;
                        }
                        auth |= line.eq_ignore_ascii_case("Authorization: Basic dGVzdDo=\r\n");
                    }
                    assert!(auth);
                    let body = reply.to_string();
                    write!(
                        stream,
                        "HTTP/1.1 {} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status,
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            });
            verify(source, deadline);
            server.join().unwrap();
        });
    }

    #[test]
    fn open_code_refresh_skips_invalid_rows_retains_messages_and_obeys_ttl() {
        let id = "ses_example";
        let row = json!({"id":id,"title":"Demo","time":{"created":1700000000000u64,"updated":1700000001000u64}});
        fixture(
            Kind::OpenCode,
            vec![
                (
                    "/api/session",
                    200,
                    json!({"data":[{"id":"not/a/session"},row]}),
                ),
                (
                    "/api/experimental/session/ses_example/export",
                    200,
                    json!({"data":{"info":row,"messages":[
                        {"type":"user","text":"question"},
                        {"type":"assistant","content":[{"type":"text","text":"response"}]}
                    ]}}),
                ),
            ],
            |source, deadline| {
                source.refresh(false, deadline).unwrap();
                let result = read(&source.path(id)).unwrap();
                assert_eq!(result.messages.len(), 2);
                assert_eq!(result.modified, Some(1700000001));
                assert!(!source.due(false));
                source.refresh(false, deadline).unwrap();
                assert!(source.url(id).ends_with("/session/ses_example"));
            },
        );
    }

    #[test]
    fn cursor_reads_all_roles_and_rejects_mismatched_id() {
        fixture(
            Kind::Cursor,
            vec![
                (
                    "/v0/agents/bc_example",
                    200,
                    json!({"id":"bc_example","name":"Heading"}),
                ),
                (
                    "/v0/agents/bc_example/conversation",
                    200,
                    json!({"id":"bc_example","messages":[
                        {"type":"user_message","text":"first"},
                        {"type":"assistant_message","text":"a"},
                        {"type":"user_message","text":"second"}
                    ]}),
                ),
                (
                    "/v0/agents/bc_example",
                    200,
                    json!({"id":"bc_example","name":"Heading"}),
                ),
                (
                    "/v0/agents/bc_example/conversation",
                    200,
                    json!({"id":"bc_wrong","messages":[]}),
                ),
            ],
            |source, deadline| {
                let http = source.http();
                let full = source
                    .detail(&http, "bc_example", None, None, deadline)
                    .ok()
                    .unwrap();
                assert_eq!(full.messages[2].text, "second");
                assert_eq!(
                    source
                        .detail(&http, "bc_example", None, None, deadline)
                        .err(),
                    Some("session ID mismatch")
                );
            },
        );
    }

    #[test]
    fn copilot_only_labels_the_prompts_and_reports_rejected_auth() {
        const ID: &str = "11111111-1111-1111-1111-111111111111";
        const ROUTE: &str = "/agents/tasks/11111111-1111-1111-1111-111111111111";
        fixture(
            Kind::Copilot,
            vec![
                (
                    ROUTE,
                    200,
                    json!({"id":ID,"sessions":[{"task_id":ID,"prompt":"work request"}]}),
                ),
                (ROUTE, 401, json!({})),
            ],
            |source, deadline| {
                let http = source.http();
                let p = source.detail(&http, ID, None, None, deadline).ok().unwrap();
                assert_eq!(p.coverage, "prompt-only");
                assert_eq!(p.messages.len(), 1);
                assert_eq!(p.messages[0].role, "user");
                assert!(source.url(ID).is_empty());
                assert_eq!(
                    source.detail(&http, ID, None, None, deadline).err(),
                    Some("login rejected")
                );
            },
        );
    }
}

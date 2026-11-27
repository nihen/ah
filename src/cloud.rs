//! Experimental read-only Codex Cloud snapshot cache.
pub(crate) mod providers;

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const API: &str = "https://chatgpt.com/backend-api/wham/tasks/";
const LINK: &str = "https://chatgpt.com/codex/tasks/";
const MAX_BODY: u64 = 8 << 20;
const INTERVAL: Duration = Duration::from_secs(60);
const BUDGET: Duration = Duration::from_secs(1);
const FOREGROUND_BUDGET: Duration = Duration::from_secs(8);
const RETRY_INTERVAL: Duration = Duration::from_secs(10);
const PAGE_SIZE: usize = 20;

struct Source {
    access_token: String,
    account: String,
    owner: String,
    /// The ChatGPT user id of the login (from its ID token), if known.
    user: Option<String>,
    root: PathBuf,
    cache: PathBuf,
}

#[derive(Deserialize)]
struct Auth {
    auth_mode: String,
    tokens: Tokens,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    account_id: String,
    login_owner_id: String,
    #[serde(default)]
    id_token: Option<String>,
}

/// The `chatgpt_user_id` claim of a Codex login's ID token. The token is
/// only read, not verified: it comes from the user's own login file.
fn chatgpt_user_id(id_token: &str) -> Option<String> {
    use base64::Engine;
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let user = claims["https://api.openai.com/auth"]["chatgpt_user_id"].as_str()?;
    safe_header(user).then(|| user.to_owned())
}

pub fn enabled() -> bool {
    crate::config::codex_cloud_enabled()
}

fn may_contact_cloud() -> bool {
    std::env::var_os("AH_NO_CLOUD_SYNC").is_none_or(|v| v != "1")
}

fn can_fetch() -> bool {
    enabled() && may_contact_cloud()
}

fn safe_header(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic())
}

fn source() -> Result<&'static Source, &'static str> {
    static SOURCE: OnceLock<Result<Source, &'static str>> = OnceLock::new();
    SOURCE.get_or_init(read_source).as_ref().map_err(|e| *e)
}

fn read_source() -> Result<Source, &'static str> {
    if !cfg!(unix) {
        return Err("Codex Cloud cache is supported on Unix only");
    }
    let home = crate::agents::common::canonical_home();
    let base = crate::config::resolve_agent_base("codex").ok_or("cannot locate Codex login")?;
    let f = open_login(&base.join("auth.json"))?;
    let mut data = Vec::new();
    f.take(1 << 20)
        .read_to_end(&mut data)
        .map_err(|_| "cannot read Codex file login")?;
    let auth: Auth = serde_json::from_slice(&data).map_err(|_| "unsupported Codex file login")?;
    let t = auth.tokens;
    if auth.auth_mode != "chatgpt"
        || !safe_header(&t.access_token)
        || !safe_header(&t.account_id)
        || !safe_header(&t.login_owner_id)
    {
        return Err("unsupported Codex file login");
    }
    let namespace = Sha256::new()
        .chain_update(t.login_owner_id.as_bytes())
        .chain_update([0])
        .chain_update(t.account_id.as_bytes())
        .finalize();
    let root = home.join(".ah-codex-cloud");
    let cache = root.join(crate::agents::common::hex(&namespace));
    Ok(Source {
        user: t.id_token.as_deref().and_then(chatgpt_user_id),
        access_token: t.access_token,
        account: t.account_id,
        owner: t.login_owner_id,
        root,
        cache,
    })
}

#[cfg(unix)]
fn open_login(path: &Path) -> Result<File, &'static str> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "cannot read Codex file login")?;
    if !f.metadata().is_ok_and(|m| {
        m.is_file()
            && m.len() <= 1 << 20
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
    }) {
        return Err("invalid Codex file login");
    }
    Ok(f)
}

#[cfg(not(unix))]
fn open_login(_: &Path) -> Result<File, &'static str> {
    Err("unsupported platform")
}

// MCP dispatches tools on one worker. Pin the active namespace for the
// duration of each request (including its Rayon jobs), never its credentials.
static MCP_CACHE: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

pub fn pin_mcp_cache() {
    let pinned = MCP_CACHE.get_or_init(|| RwLock::new(None));
    let cache = enabled()
        .then(read_source)
        .and_then(Result::ok)
        .and_then(|s| checked_cache(&s).map(Path::to_owned));
    *pinned.write().unwrap_or_else(|e| e.into_inner()) = cache;
}

fn checked_cache(s: &Source) -> Option<&Path> {
    (private_dir(&s.root) && private_dir(&s.cache)).then_some(&s.cache)
}

pub fn cache_dir() -> Option<PathBuf> {
    if !enabled() {
        return None;
    }
    if let Some(pinned) = MCP_CACHE.get() {
        return pinned.read().unwrap_or_else(|e| e.into_inner()).clone();
    }
    checked_cache(source().ok()?).map(Path::to_owned)
}

pub fn is_cache_path(path: &Path) -> bool {
    if !path
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(valid_id)
        || path.extension().is_none_or(|s| s != "jsonl")
    {
        return false;
    }
    let (Some(cache), Some(parent)) = (cache_dir(), path.parent()) else {
        return false;
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    (parent == cache || fs::canonicalize(parent).is_ok_and(|p| p == cache))
        && fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheRoot {
    Legacy,
    Provider,
}

fn cache_root_of(path: &Path, home: &Path) -> Option<CacheRoot> {
    let relative = path.strip_prefix(home).ok()?;
    match relative.components().next()?.as_os_str().to_str()? {
        ".ah-codex-cloud" => Some(CacheRoot::Legacy),
        ".ah-provider-cloud" => Some(CacheRoot::Provider),
        _ => None,
    }
}

/// Do not allow unavailable Cloud snapshots to fall through to the generic
/// file parser. Canonicalizing detects aliases that hide the cache prefix.
pub(crate) fn validate_cache_ref(path: PathBuf, home: &Path) -> Result<PathBuf, String> {
    let lexical = if path.is_absolute() {
        cache_root_of(&path, home)
    } else {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| cache_root_of(&cwd.join(&path), home))
    };
    let canonical = fs::canonicalize(&path).ok();
    let physical = canonical.as_deref().and_then(|p| cache_root_of(p, home));
    let direct_symlink = || fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink());
    let inside_cache = lexical.or_else(|| {
        if physical.is_none() && !direct_symlink() {
            return None;
        }
        let parent = path.parent()?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let resolved = fs::canonicalize(parent).ok()?;
        cache_root_of(&resolved, home)
    });
    if inside_cache.is_none() && physical.is_none() {
        return Ok(path);
    }
    let valid = canonical.filter(|p| {
        // An in-cache symlink is never a valid snapshot, even if it points
        // at another file in the same cache. Outside aliases can name a
        // validated canonical snapshot without bypassing its parser.
        if inside_cache.is_some()
            && (inside_cache != physical
                || fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()))
        {
            return false;
        }
        match physical {
            Some(CacheRoot::Legacy) => {
                is_cache_path(p) && crate::config::active_agents().any(|a| a.id == "codex")
            }
            Some(CacheRoot::Provider) => providers::source_for_path(p).is_some_and(|source| {
                crate::config::active_agents().any(|a| a.id == source.agent())
            }),
            None => false,
        }
    });
    valid.ok_or_else(|| "Cloud snapshot is not available for the active account or mode.".into())
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("task_e_")
        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn url_id(value: &str) -> Option<&str> {
    value.strip_prefix(LINK).filter(|s| valid_id(s))
}

pub fn url_for_path(path: &Path) -> String {
    if !is_cache_path(path) {
        return String::new();
    }
    format!(
        "{LINK}{}",
        path.file_stem().unwrap_or_default().to_string_lossy()
    )
}

#[cfg(unix)]
fn private_dir(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0)
}

#[cfg(not(unix))]
fn private_dir(_: &Path) -> bool {
    false
}

#[cfg(unix)]
fn lock_dirs(root: &Path, cache: &Path) -> Result<Option<File>, &'static str> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    for p in [root, cache] {
        match fs::DirBuilder::new().mode(0o700).create(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("cannot create Cloud cache"),
        }
        if !private_dir(p) {
            return Err("Cloud cache directory is not private");
        }
    }
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(cache.join(".lock"))
        .map_err(|_| "cannot open Cloud cache lock")?;
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Err("invalid Cloud cache lock");
    }
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(f))
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err("cannot lock Cloud cache")
    }
}

#[cfg(not(unix))]
fn lock_dirs(_: &Path, _: &Path) -> Result<Option<File>, &'static str> {
    Ok(None)
}

fn lock(s: &Source) -> Result<Option<File>, &'static str> {
    lock_dirs(&s.root, &s.cache)
}

#[derive(Deserialize)]
struct Page {
    items: Vec<Task>,
}

#[derive(Deserialize)]
struct Task {
    id: String,
    updated_at: f64,
}

#[derive(Deserialize)]
struct Detail {
    task: TaskMetadata,
    current_user_turn: Turn,
    current_assistant_turn: Option<Turn>,
}

#[derive(Deserialize)]
struct TaskMetadata {
    id: String,
    title: Option<String>,
    created_at: f64,
}

#[derive(Deserialize)]
struct Turn {
    created_at: f64,
    input_items: Option<Vec<Value>>,
    output_items: Option<Vec<Value>>,
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .proxy(None)
        .https_only(true)
        .build()
        .into()
}

fn get<T: serde::de::DeserializeOwned>(
    agent: &ureq::Agent,
    s: &Source,
    suffix: &str,
    deadline: Instant,
) -> Result<T, &'static str> {
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or("Cloud refresh timed out")?;
    let mut res = agent
        .get(format!("{API}{suffix}"))
        .header("Authorization", format!("Bearer {}", s.access_token))
        .header("ChatGPT-Account-Id", &s.account)
        .header("User-Agent", "ah-cli/codex-cloud")
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(401 | 403) => "Codex file login rejected; check Codex login",
            ureq::Error::StatusCode(404) => "Cloud task not found or not accessible",
            _ => "Cloud request failed or timed out",
        })?;
    if res.status() != 200 {
        return Err("unexpected Cloud HTTP response");
    }
    let mut data = Vec::new();
    res.body_mut()
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut data)
        .map_err(|_| "cannot read Cloud response")?;
    if data.len() as u64 > MAX_BODY {
        return Err("Cloud response exceeds size limit");
    }
    serde_json::from_slice(&data).map_err(|_| "unsupported Cloud response")
}

pub fn epoch(seconds: f64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::try_from_secs_f64(seconds).ok()?)
}

fn store(
    s: &Source,
    id: &str,
    detail: Detail,
    updated: Option<f64>,
) -> Result<PathBuf, &'static str> {
    if !valid_id(id) || detail.task.id != id {
        return Err("Cloud response task ID mismatch");
    }
    let rows = [
        (
            detail.current_user_turn.input_items,
            "user",
            "input_text",
            detail.current_user_turn.created_at,
        ),
        detail
            .current_assistant_turn
            .map_or((None, "assistant", "output_text", 0.0), |a| {
                (a.output_items, "assistant", "output_text", a.created_at)
            }),
    ];
    let modified = updated.unwrap_or_else(|| rows[0].3.max(rows[1].3));
    let mtime = epoch(modified).ok_or("invalid Cloud update time")?;
    epoch(detail.task.created_at).ok_or("invalid Cloud creation time")?;
    let mut f =
        tempfile::NamedTempFile::new_in(&s.cache).map_err(|_| "cannot stage Cloud snapshot")?;
    // Bound the first JSONL record, which is read with a 16 KiB cap.
    let title = detail.task.title.as_deref().map(|t| {
        crate::output::strip_ansi(t)
            .chars()
            .take(512)
            .collect::<String>()
    });
    let header = json!({"type":"session_meta","payload":{
        "id":id, "cloud_title":title, "cloud_created_at":detail.task.created_at, "cloud_list_updated_at":updated
    }});
    let write = |f: &mut tempfile::NamedTempFile, record: Value| -> Result<(), &'static str> {
        serde_json::to_writer(&mut *f, &record).map_err(|_| "cannot write Cloud snapshot")?;
        f.write_all(b"\n")
            .map_err(|_| "cannot write Cloud snapshot")
    };
    write(&mut f, header)?;
    let mut has_user = false;
    for (items, role, kind, stamp) in rows {
        let timestamp = epoch(stamp)
            .and_then(|t| jiff::Timestamp::try_from(t).ok())
            .map(|t| t.display_with_offset(jiff::tz::Offset::UTC).to_string());
        for item in items.into_iter().flatten() {
            if item.get("type").and_then(Value::as_str) != Some("message")
                || item.get("role").and_then(Value::as_str) != Some(role)
            {
                continue;
            }
            let parts: Vec<_> = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.trim().is_empty())
                .map(|text| json!({"type":kind,"text":text}))
                .collect();
            if parts.is_empty() {
                continue;
            }
            has_user |= role == "user";
            write(
                &mut f,
                json!({"type":"response_item","timestamp":timestamp,"payload":{
                    "type":"message","role":role,"content":parts,"ah_cloud_text":true
                }}),
            )?;
        }
    }
    if !has_user {
        return Err("Cloud response has no user text");
    }
    f.as_file()
        .set_times(FileTimes::new().set_modified(mtime))
        .map_err(|_| "cannot set Cloud snapshot timestamps")?;
    let path = s.cache.join(format!("{id}.jsonl"));
    f.persist(&path)
        .map_err(|_| "cannot publish Cloud snapshot")?;
    Ok(path)
}

fn updated_on_disk(path: &Path, updated: f64) -> bool {
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let version = || {
        let mut line = String::new();
        BufReader::new(File::open(path).ok()?.take(16 << 10))
            .read_line(&mut line)
            .ok()?;
        let row: Value = serde_json::from_str(&line).ok()?;
        row.pointer("/payload/cloud_list_updated_at")?.as_f64()
    };
    version() == Some(updated)
}

fn recent_stamp(path: &Path, window: Duration) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < window)
}

fn write_stamp(cache: &Path, name: &str) -> Result<(), &'static str> {
    tempfile::NamedTempFile::new_in(cache)
        .and_then(|f| f.persist(cache.join(name)).map_err(|e| e.error))
        .map(drop)
        .map_err(|_| "cannot write Cloud refresh stamp")
}

fn refresh(force: bool, deadline: Instant) -> Result<(), &'static str> {
    let s = source()?;
    let Some(_lock) = lock(s)? else {
        return if force {
            Err("Cloud cache is busy")
        } else {
            Ok(())
        };
    };
    if !force
        && (recent_stamp(&s.cache.join(".last-success"), INTERVAL)
            || recent_stamp(&s.cache.join(".last-attempt"), RETRY_INTERVAL))
    {
        return Ok(());
    }
    write_stamp(&s.cache, ".last-attempt")?;
    let agent = http();
    let page: Page = get(&agent, s, "list?limit=20", deadline)?;
    refresh_page(s, page, deadline, |id| get(&agent, s, id, deadline))?;
    write_stamp(&s.cache, ".last-success")
}

fn refresh_page(
    s: &Source,
    page: Page,
    deadline: Instant,
    mut fetch: impl FnMut(&str) -> Result<Detail, &'static str>,
) -> Result<(), &'static str> {
    if page.items.len() > PAGE_SIZE {
        return Err("Cloud task list exceeds page size");
    }
    let mut failure = None;
    for task in page.items {
        if !valid_id(&task.id) {
            failure = Some("Cloud task list contains unsupported task IDs");
            continue;
        }
        if epoch(task.updated_at).is_none() {
            failure = Some("Cloud task has invalid update time");
            continue;
        }
        if updated_on_disk(&s.cache.join(format!("{}.jsonl", task.id)), task.updated_at) {
            continue;
        }
        if Instant::now() >= deadline {
            return Err("Cloud refresh deadline reached; some tasks will retry");
        }
        if let Err(e) = fetch(&task.id)
            .and_then(|detail| store(s, &task.id, detail, Some(task.updated_at)).map(drop))
        {
            failure = Some(e);
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(())
}

pub fn refresh_for_log(offline: bool, force: bool, agent: Option<&str>) {
    if offline || !may_contact_cloud() {
        return;
    }
    let legacy = can_fetch()
        && agent.is_none_or(|a| a == "codex")
        && crate::config::active_agents().any(|a| a.id == "codex");
    let configured = providers::active(agent);
    let sources: Vec<_> = configured
        .iter()
        .copied()
        .filter(|s| s.due(force))
        .collect();
    if !legacy && configured.is_empty() {
        if force {
            eprintln!("Warning: no enabled cloud source matches this agent");
        }
        return;
    }
    let budget = if force {
        FOREGROUND_BUDGET
    } else if sources.is_empty() {
        BUDGET
    } else {
        Duration::from_secs(3)
    };
    let end = Instant::now() + budget;
    std::thread::scope(|scope| {
        if legacy {
            scope.spawn(move || {
                if let Err(e) = refresh(
                    force,
                    (Instant::now() + if force { budget } else { BUDGET }).min(end),
                ) {
                    eprintln!(
                        "Warning: Codex Cloud (tasks): {} (using saved history)",
                        crate::output::sanitize_multiline(e)
                    );
                }
            });
        }
        for source in sources {
            scope.spawn(move || {
                if let Err(e) = source.refresh(force, end) {
                    eprintln!(
                        "Warning: {}: {} (using saved history)",
                        source.name(),
                        crate::output::sanitize_multiline(e)
                    );
                }
            });
        }
    });
}

pub fn any_enabled() -> bool {
    enabled() || !providers::active(None).is_empty()
}

pub fn fetch_missing(
    id: &str,
    offline: bool,
    agent: Option<&str>,
) -> Result<Option<PathBuf>, String> {
    if offline
        || !can_fetch()
        || !valid_id(id)
        || agent.is_some_and(|a| a != "codex")
        || !crate::config::active_agents().any(|a| a.id == "codex")
    {
        return Ok(None);
    }
    let fetch = || {
        let deadline = Instant::now() + FOREGROUND_BUDGET;
        let s = source()?;
        let Some(_lock) = lock(s)? else {
            return Err("Cloud cache is busy");
        };
        let agent = http();
        let detail = get(&agent, s, id, deadline)?;
        store(s, id, detail, None)
    };
    fetch()
        .map(Some)
        .map_err(|e| format!("Could not check Codex Cloud: {e}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const ID: &str = "task_e_e0000000000000000000000000000000";
    const NEW: &str = "task_e_f0000000000000000000000000000000";

    fn example(id: &str) -> Detail {
        serde_json::from_value(json!({
            "task":{"id":id, "created_at":1.0, "title":"Example"},
            "current_user_turn":{"input_items":[
                {"type":"message", "role":"user", "content":[{"text":"# Actual brief"}]},
                {"type":"other", "role":"user", "content":[{"text":"Not a message"}]}
            ], "created_at":2.0},
            "current_assistant_turn":{"output_items":[
                {"type":"message","role":"assistant","content":[{"text":"Completed"}]}
            ],"created_at":3.0}
        }))
        .unwrap()
    }

    fn source_in(dir: &Path) -> Source {
        let root = dir.join("cache");
        Source {
            access_token: "example".into(),
            account: "example".into(),
            owner: "owner".into(),
            user: None,
            cache: root.join("example"),
            root,
        }
    }

    #[test]
    fn accepts_only_full_codex_task_ids() {
        assert_eq!(url_id(&format!("{LINK}{ID}")), Some(ID));
        for bad in [
            "/local/",
            "task_e_e000",
            "task_e_../../../../",
            "https://other.test/",
        ] {
            assert!(!valid_id(bad));
            assert_eq!(url_id(bad), None);
        }
        assert!(url_id(&format!("{LINK}{ID}?query=1")).is_none());
    }

    #[test]
    fn cloud_cache_publishes_atomically_and_retries_only_unsaved_updates() {
        let dir = tempfile::tempdir().unwrap();
        let s = source_in(dir.path());
        let _lock = lock(&s).unwrap().unwrap();
        assert!(lock(&s).unwrap().is_none());
        let mut detail = example(ID);
        detail.task.title = Some("long title".repeat(4096));
        let p = store(&s, ID, detail, Some(5.0)).unwrap();
        assert!(updated_on_disk(&p, 5.0));
        assert_eq!(p.metadata().unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(
            p.metadata().unwrap().modified().unwrap(),
            epoch(5.).unwrap()
        );
        let old = fs::read(&p).unwrap();
        assert!(store(&s, ID, example(NEW), Some(6.)).is_err());

        let page = |updated| Page {
            items: vec![
                Task {
                    id: ID.into(),
                    updated_at: updated,
                },
                Task {
                    id: NEW.into(),
                    updated_at: 9.,
                },
            ],
        };
        assert!(
            refresh_page(&s, page(5.), Instant::now() + BUDGET, |id| {
                assert_eq!(id, NEW);
                Err("not available")
            })
            .is_err()
        );
        assert_eq!(fs::read(&p).unwrap(), old);
        assert!(!s.cache.join(format!("{NEW}.jsonl")).exists());
        let mut requested = Vec::new();
        refresh_page(&s, page(6.), Instant::now() + BUDGET, |id| {
            requested.push(id.to_owned());
            Ok(example(id))
        })
        .unwrap();
        assert_eq!(requested, [ID, NEW]);
        assert_eq!(
            p.metadata().unwrap().modified().unwrap(),
            epoch(6.).unwrap()
        );
        assert!(
            !String::from_utf8(fs::read(p).unwrap())
                .unwrap()
                .contains("Not a message")
        );
    }

    #[test]
    fn refuses_symlink_or_public_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let fixture = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        symlink(fixture.path(), &path).unwrap();
        assert!(open_login(&path).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_login(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(open_login(&path).is_ok());
    }

    #[test]
    fn refuses_symlink_or_public_directory() {
        let d = tempfile::tempdir().unwrap();
        let s = source_in(d.path());
        symlink(d.path(), &s.root).unwrap();
        assert!(lock(&s).is_err());
        fs::remove_file(&s.root).unwrap();
        fs::create_dir(&s.root).unwrap();
        fs::set_permissions(&s.root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(lock(&s).is_err());
    }
}

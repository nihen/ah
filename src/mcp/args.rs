//! Tool argument parsing and merging with the server's default filters.
use serde_json::{Map, Value, json};

use crate::cli::FilterArgs;

/// Typed accessors over a tool's `arguments` object. Every getter returns
/// `Ok(None)` when the key is absent and an error when it has the wrong type.
pub struct Params<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Params<'a> {
    pub fn new(map: &'a Map<String, Value>) -> Self {
        Self { map }
    }

    pub fn has(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// Reject keys outside `allowed` so a misspelled argument is an error
    /// instead of silently ignored.
    pub fn check_allowed(&self, allowed: &[&str]) -> Result<(), String> {
        let unknown: Vec<&str> = self
            .map
            .keys()
            .map(String::as_str)
            .filter(|k| !allowed.contains(k))
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "unknown argument(s): {}. allowed: {}",
                unknown.join(", "),
                allowed.join(", ")
            ))
        }
    }

    pub fn bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(format!("'{}' must be a boolean", key)),
        }
    }

    pub fn string(&self, key: &str) -> Result<Option<String>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("'{}' must be a string", key)),
        }
    }

    pub fn required_string(&self, key: &str) -> Result<String, String> {
        match self.string(key)? {
            Some(s) if !s.is_empty() => Ok(s),
            _ => Err(format!("'{}' is required", key)),
        }
    }

    /// A non-negative integer.
    pub fn uint(&self, key: &str) -> Result<Option<u64>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => n
                .as_u64()
                .map(Some)
                .ok_or_else(|| format!("'{}' must be a non-negative integer", key)),
            Some(_) => Err(format!("'{}' must be a non-negative integer", key)),
        }
    }

    pub fn string_list(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("'{}' must be an array of strings", key))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(_) => Err(format!("'{}' must be an array of strings", key)),
        }
    }
}

/// Common filter argument names, in the order the schemas list them.
pub const ALL: &str = "all";
pub const DIR: &str = "dir";
pub const AGENT: &str = "agent";
pub const PROJECT: &str = "project";
pub const SINCE: &str = "since";
pub const UNTIL: &str = "until";
pub const RUNNING: &str = "running";
pub const NO_ARCHIVED: &str = "no_archived";
pub const SUBAGENTS: &str = "subagents";
pub const LIMIT: &str = "limit";

/// Overlay the common filter arguments present in `params` on the server's
/// startup filters. Only the keys in `allowed` are read. A present key always
/// wins (`false`/`0` included); `dir` clears `all` and `all: true` clears
/// `dir`, and both in one call is an error.
pub fn merge_filter(
    base: &FilterArgs,
    params: &Params<'_>,
    allowed: &[&str],
) -> Result<FilterArgs, String> {
    let mut f = base.clone();
    // Never inherited: rejected at startup, and search sets its own.
    f.query = None;
    f.prompt_only = false;
    f.raw_search = false;

    let has = |k: &str| allowed.contains(&k) && params.has(k);
    let all = if has(ALL) { params.bool(ALL)? } else { None };
    let dir = if has(DIR) { params.string(DIR)? } else { None };
    if all == Some(true) && dir.is_some() {
        return Err("'all' and 'dir' are exclusive; pass one of them".into());
    }
    if let Some(all) = all {
        f.all = all;
        if all {
            f.dir = None;
        }
    }
    if let Some(dir) = dir {
        if dir.is_empty() {
            return Err("'dir' must not be empty".into());
        }
        f.dir = Some(dir);
        f.all = false;
    }
    if has(AGENT) {
        f.agent = params.string(AGENT)?.filter(|s| !s.is_empty());
    }
    if has(PROJECT) {
        f.project = params.string(PROJECT)?.filter(|s| !s.is_empty());
    }
    if has(SINCE) {
        f.since = params.string(SINCE)?.filter(|s| !s.is_empty());
    }
    if has(UNTIL) {
        f.until = params.string(UNTIL)?.filter(|s| !s.is_empty());
    }
    if has(RUNNING) {
        f.running = params.bool(RUNNING)?.unwrap_or(false);
    }
    if has(NO_ARCHIVED) {
        f.no_archived = params.bool(NO_ARCHIVED)?.unwrap_or(false);
    }
    if has(SUBAGENTS) {
        f.subagents = params.bool(SUBAGENTS)?.unwrap_or(false);
    }
    if has(LIMIT) {
        f.limit = params.uint(LIMIT)?.unwrap_or(0) as usize;
    }
    // Validate time specs now so the error names the argument.
    f.since_time().map_err(|e| format!("'since': {}", e))?;
    f.until_time().map_err(|e| format!("'until': {}", e))?;
    Ok(f)
}

/// The directory scope a filter applies: `{dir, all}`. `dir` is the
/// canonical directory (the server cwd when neither `-a` nor `-d` applies).
pub fn scope_json(filter: &FilterArgs) -> Value {
    if filter.all {
        json!({ "dir": Value::Null, "all": true })
    } else {
        let dir = FilterArgs::resolve_dir(filter.dir.as_deref().unwrap_or("."));
        json!({ "dir": dir, "all": false })
    }
}

/// Scope of a tool that only narrows by `dir` (`project`): without `dir`
/// every directory is covered, whatever `-a` says.
pub fn dir_only_scope_json(filter: &FilterArgs) -> Value {
    match &filter.dir {
        Some(d) => json!({ "dir": FilterArgs::resolve_dir(d), "all": false }),
        None => json!({ "dir": Value::Null, "all": true }),
    }
}

/// JSON Schema fragments for the common filter arguments.
pub fn common_schema(name: &str) -> Value {
    match name {
        ALL => {
            json!({"type": "boolean", "description": "Search every directory instead of the default scope (exclusive with dir)"})
        }
        DIR => {
            json!({"type": "string", "description": "Only sessions whose working directory is this path (exclusive with all)"})
        }
        AGENT => {
            json!({"type": "string", "description": "Only this agent (e.g. claude, codex, gemini); see list_agents"})
        }
        PROJECT => {
            json!({"type": "string", "description": "Only this project name (usually the basename of the working directory). In log, search and memory it narrows within the directory scope, like the CLI's --project; add all: true there to search every directory"})
        }
        SINCE => {
            json!({"type": "string", "description": "Only sessions modified after this time: YYYY-MM-DD, 'YYYY-MM-DD HH:MM', or relative like 3d, 1w, 12h, 2m (~60 days)"})
        }
        UNTIL => {
            json!({"type": "string", "description": "Only sessions modified before this time (same formats as since)"})
        }
        RUNNING => {
            json!({"type": "boolean", "description": "Only sessions whose agent process is currently running"})
        }
        NO_ARCHIVED => {
            json!({"type": "boolean", "description": "Hide sessions the agent archived (Codex)"})
        }
        SUBAGENTS => {
            json!({"type": "boolean", "description": "Include subagent sessions spawned by another session"})
        }
        LIMIT => {
            json!({"type": "integer", "minimum": 0, "description": "Scan at most this many newest session files (0 = no limit); this bounds the scan, not the result count"})
        }
        _ => unreachable!("unknown common argument {name}"),
    }
}

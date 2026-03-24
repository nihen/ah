use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::first_text_part;
use super::common::for_each_jsonl_value;

static RE_CURSOR_PROJECTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".*/projects/([^/]+)/.*").unwrap());

static DECODE_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Decode Cursor's dash-encoded directory name to an actual filesystem path.
/// e.g. "Users-you-src-github-com-org-repo" -> "/Users/you/src/github.com/org/repo"
/// Greedy: at each `-`, pick the first separator (`/`, `.`, `_`, `-`) that yields
/// an existing directory prefix. Falls back to `/` if nothing exists.
/// Results are cached per encoded string.
fn decode_cursor_path(encoded: &str) -> Option<String> {
    if let Ok(cache) = DECODE_CACHE.lock() {
        if let Some(result) = cache.get(encoded) {
            return result.clone();
        }
    }
    let result = decode_cursor_path_inner(encoded);
    if let Ok(mut cache) = DECODE_CACHE.lock() {
        cache.insert(encoded.to_string(), result.clone());
    }
    result
}

fn decode_cursor_path_inner(encoded: &str) -> Option<String> {
    // Cursor encodes paths by replacing `/`, `.`, `_` with `-`.
    // On Linux, paths start with "data-" prefix (e.g. "data-home-user-...")
    // On macOS, paths start with "Users-..." or similar.

    let encoded = if let Some(rest) = encoded.strip_prefix("data-") {
        rest
    } else {
        encoded
    };

    // Step 1: replace all `-` with `/`
    let stripped = encoded.strip_prefix('-').unwrap_or(encoded);
    let simple = format!("/{}", stripped.replace('-', "/"));
    if PathBuf::from(&simple).exists() {
        return Some(simple);
    }

    // Step 2: try domain TLD patterns (.com, .org, .io, .jp)
    let with_dots = simple
        .replace("/com/", ".com/")
        .replace("/org/", ".org/")
        .replace("/io/", ".io/")
        .replace("/jp/", ".jp/");
    if with_dots != simple && PathBuf::from(&with_dots).exists() {
        return Some(with_dots);
    }

    // Step 3: try merging adjacent path components with `-` or `_`
    // (e.g. /org/repo -> /org-repo, /My/App -> /My_App)
    // Try merging from the org/repo level (after domain) backwards
    let base = if with_dots != simple {
        &with_dots
    } else {
        &simple
    };
    let components: Vec<&str> = base.split('/').collect();
    // Try merging pairs from right to left (limited attempts)
    for i in (2..components.len()).rev() {
        for sep in ["-", "_"] {
            let merged = format!("{}{}{}", components[i - 1], sep, components[i]);
            let mut attempt: Vec<&str> = components[..i - 1].to_vec();
            attempt.push(&merged);
            attempt.extend_from_slice(&components[i + 1..]);
            let path = attempt.join("/");
            if PathBuf::from(&path).exists() {
                return Some(path);
            }
        }
    }

    // Fallback
    Some(base.to_string())
}

pub static PLUGIN: CursorPlugin = CursorPlugin;

pub struct CursorPlugin;

/// Cursor transcripts wrap real user text in `<user_query>…</user_query>`; strip for title / prompts.
fn cursor_user_body(raw: &str) -> Option<&str> {
    let t = raw.trim();
    if let (Some(i), Some(j)) = (t.find("<user_query>"), t.rfind("</user_query>")) {
        let start = i + "<user_query>".len();
        if j >= start {
            let inner = t[start..j].trim();
            if !inner.is_empty() {
                return Some(inner);
            }
        }
    }
    if !t.starts_with('<') {
        return Some(t);
    }
    None
}

impl AgentPlugin for CursorPlugin {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn description(&self) -> &'static str {
        "Cursor Agent"
    }

    fn can_resume(&self) -> bool {
        true
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

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.cursor/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        for_each_jsonl_value(path, |val| {
            match val.get("role").and_then(|v| v.as_str()) {
                Some("user") => {
                    if let Some(raw) = val.get("message").and_then(first_text_part) {
                        if let Some(text) = cursor_user_body(raw) {
                            return visit(Message::user(text.to_string()));
                        }
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

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec![
            "cursor-agent".to_string(),
            "-p".to_string(),
            "--resume".to_string(),
            id,
        ])
    }
}

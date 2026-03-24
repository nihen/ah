use std::fs;
use std::path::Path;
use std::sync::LazyLock;
use std::time::SystemTime;

use regex::Regex;
use sha2::{Digest, Sha256};

use super::AgentPlugin;
use super::Message;
use super::common::first_text_part;
use super::common::format_mtime;
use super::common::mmap_file;

static RE_GEMINI_TMP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r".*/tmp/([^/]+)/.*").unwrap());
static RE_GEMINI_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"session-(\d{4}-\d{2}-\d{2})T(\d{2})-(\d{2})").unwrap());

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

pub static PLUGIN: GeminiPlugin = GeminiPlugin;

pub struct GeminiPlugin;

impl GeminiPlugin {
    fn for_each_root_message(path: &Path, mut visit: impl FnMut(&serde_json::Value) -> bool) {
        let mmap = match mmap_file(path) {
            Some(mmap) => mmap,
            None => return,
        };

        let root = match serde_json::from_slice::<serde_json::Value>(&mmap) {
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
            ".gemini/tmp/*/logs.json",
        ]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.gemini/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        Self::for_each_root_message(path, |val| {
            match val.get("type").and_then(|v| v.as_str()) {
                Some("user") => {
                    // chats/session-*.json uses "content", logs.json uses "message"
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
                    if let Some(text) = val.get("content").and_then(first_text_part) {
                        return visit(Message::assistant(text.to_string()));
                    }
                }
                _ => {}
            }
            true
        });
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
            .map(|caps| format!("{} {}:{}", &caps[1], &caps[2], &caps[3]))
            .or_else(|| Some(format_mtime(mtime)))
    }

    fn resolve_cwd(&self, path: &Path, home: &Path) -> Option<String> {
        let path_str = path.to_string_lossy();
        let project = RE_GEMINI_TMP.captures(&path_str)?.get(1)?.as_str();

        // Derive gemini base from session file path (supports GEMINI_CLI_HOME override).
        // Walk up from the file until we find {tmp,history}/{project} and take the parent of tmp/history.
        // This handles both:
        //   {base}/tmp/{project}/chats/session-*.json
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

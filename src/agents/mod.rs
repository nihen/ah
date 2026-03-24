pub(crate) mod claude;
mod codex;
pub(crate) mod common;
mod copilot;
mod cursor;
mod gemini;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: MessageRole,
    pub text: String,
}

impl Message {
    pub fn user(text: String) -> Self {
        Self {
            role: MessageRole::User,
            text,
        }
    }

    pub fn assistant(text: String) -> Self {
        Self {
            role: MessageRole::Assistant,
            text,
        }
    }
}

pub trait AgentPlugin: Sync {
    fn id(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn glob_patterns(&self) -> &'static [&'static str];
    fn path_markers(&self) -> &'static [&'static str];

    fn can_search(&self) -> bool {
        true
    }
    fn can_show(&self) -> bool {
        true
    }
    fn can_resume(&self) -> bool {
        false
    }
    fn can_detect_running(&self) -> bool {
        false
    }
    fn can_memory(&self) -> bool {
        false
    }
    fn can_follow(&self) -> bool {
        false
    }

    /// How project is resolved, for list-agents display
    fn project_desc(&self) -> &'static str {
        "parent directory name of session file"
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool);

    /// Iterate messages from pre-loaded bytes (avoids redundant mmap/file open).
    /// Default falls back to iter_messages (re-reads file).
    fn iter_messages_from_bytes(
        &self,
        path: &Path,
        _data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        self.iter_messages(path, visit);
    }

    /// Extract messages from a single JSON value (one JSONL line).
    /// Used by follow mode to process new lines incrementally.
    /// Default: no-op. Plugins should override if they support follow.
    fn messages_from_value(&self, _val: &serde_json::Value) -> Vec<Message> {
        Vec::new()
    }

    /// Return the file path to use for full-text search (mmap).
    /// Defaults to the session file itself. Override when the searchable
    /// content lives in a different file (e.g. Copilot events.jsonl).
    fn search_path(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }

    fn resolve_project(&self, _path: &Path, _home: &Path) -> Option<String> {
        None
    }

    fn resolve_date(&self, _path: &Path, _mtime: SystemTime) -> Option<String> {
        None
    }

    fn resolve_cwd(&self, _path: &Path, _home: &Path) -> Option<String> {
        None
    }

    /// Resolve cwd from pre-loaded mmap data (avoids re-opening the file).
    /// Default falls back to resolve_cwd.
    fn resolve_cwd_from_mmap(&self, path: &Path, home: &Path, _mmap: &[u8]) -> Option<String> {
        self.resolve_cwd(path, home)
    }

    fn resolve_title(&self, _path: &Path, _home: &Path) -> Option<String> {
        None
    }

    /// Resolve title from pre-loaded mmap data (avoids re-mmapping the file).
    /// Default falls back to resolve_title.
    fn resolve_title_from_mmap(&self, path: &Path, home: &Path, _mmap: &[u8]) -> Option<String> {
        self.resolve_title(path, home)
    }

    fn resolve_resume_id(&self, _path: &Path, _home: &Path) -> Option<String> {
        None
    }

    fn resume_args(&self, _path: &Path, _home: &Path) -> Option<Vec<String>> {
        None
    }
}

struct UnknownPlugin;

impl AgentPlugin for UnknownPlugin {
    fn id(&self) -> &'static str {
        "unknown"
    }

    fn description(&self) -> &'static str {
        "Unknown agent"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &[]
    }

    fn can_search(&self) -> bool {
        false
    }
    fn can_show(&self) -> bool {
        false
    }

    fn iter_messages(&self, _path: &Path, _visit: &mut dyn FnMut(Message) -> bool) {}
}

static UNKNOWN_PLUGIN: UnknownPlugin = UnknownPlugin;
static PLUGINS: [&'static dyn AgentPlugin; 5] = [
    &claude::PLUGIN,
    &codex::PLUGIN,
    &gemini::PLUGIN,
    &copilot::PLUGIN,
    &cursor::PLUGIN,
];

pub fn all_plugins() -> &'static [&'static dyn AgentPlugin] {
    &PLUGINS
}

pub fn find_builtin_plugin(id: &str) -> Option<&'static dyn AgentPlugin> {
    all_plugins()
        .iter()
        .copied()
        .find(|plugin| plugin.id() == id)
}

#[cfg(test)]
pub fn find_plugin(id: &str) -> Option<&'static dyn AgentPlugin> {
    find_builtin_plugin(id)
}

pub fn unknown_plugin() -> &'static dyn AgentPlugin {
    &UNKNOWN_PLUGIN
}

pub fn find_plugin_for_path(path: &Path) -> &'static dyn AgentPlugin {
    crate::config::find_plugin_for_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_plugin() {
        assert_eq!(find_plugin("claude").unwrap().id(), "claude");
        assert_eq!(find_plugin("codex").unwrap().id(), "codex");
        assert_eq!(find_plugin("gemini").unwrap().id(), "gemini");
        assert_eq!(find_plugin("copilot").unwrap().id(), "copilot");
        assert_eq!(find_plugin("cursor").unwrap().id(), "cursor");
        assert!(find_plugin("foobar").is_none());
    }

    #[test]
    fn test_find_plugin_for_path() {
        crate::config::init(Path::new("/nonexistent/home"));
        assert_eq!(
            find_plugin_for_path(Path::new("/home/user/.claude/projects/foo/bar.jsonl")).id(),
            "claude"
        );
        assert_eq!(
            find_plugin_for_path(Path::new(
                "/home/user/.codex/sessions/2026/01/01/rollout.jsonl"
            ))
            .id(),
            "codex"
        );
        assert_eq!(
            find_plugin_for_path(Path::new("/home/user/.gemini/tmp/proj/chats/session.json")).id(),
            "gemini"
        );
        assert_eq!(
            find_plugin_for_path(Path::new(
                "/home/user/.copilot/session-state/uuid/workspace.yaml"
            ))
            .id(),
            "copilot"
        );
        assert_eq!(
            find_plugin_for_path(Path::new(
                "/home/user/.cursor/projects/foo/agent-transcripts/s.jsonl"
            ))
            .id(),
            "cursor"
        );
        assert_eq!(
            find_plugin_for_path(Path::new("/tmp/random.txt")).id(),
            "unknown"
        );
    }
}

mod agy;
pub(crate) mod claude;
mod codex;
pub(crate) mod common;
mod copilot;
mod cursor;
mod gemini;
mod grok;
mod opencode;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use common::{SessionBytes, mmap_file};

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

    /// Expand a file matched by `glob_patterns` into sessions.
    /// Returns `None` when the file itself is the session (the default).
    /// Plugins whose sessions live inside a container file (e.g. rows of a
    /// SQLite database) return virtual paths such as `<db>/<session-id>`
    /// with each session's modification time. Virtual paths never exist on
    /// disk, so every filesystem access must go through the hooks below.
    fn expand_sessions(&self, _file: &Path) -> Option<Vec<(PathBuf, SystemTime)>> {
        None
    }

    /// Modification time of a session; `None` when the session does not exist.
    fn session_mtime(&self, path: &Path) -> Option<SystemTime> {
        fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    fn session_created(&self, path: &Path) -> Option<SystemTime> {
        fs::metadata(path).and_then(|m| m.created()).ok()
    }

    fn session_size(&self, path: &Path) -> Option<u64> {
        fs::metadata(path).map(|m| m.len()).ok()
    }

    /// Whether this file is only a secondary record of its session (e.g.
    /// Gemini's per-project `logs.json` prompt log). When several files
    /// share a session id, a dedicated session file wins over such a copy.
    fn is_secondary_record(&self, _path: &Path) -> bool {
        false
    }

    /// Whether the agent has archived this session (hidden from its own
    /// session picker but still resumable).
    fn is_archived(&self, _path: &Path) -> bool {
        false
    }

    /// Bytes used for full-text search. Defaults to mmapping `search_path`.
    fn session_bytes(&self, path: &Path) -> Option<SessionBytes> {
        mmap_file(&self.search_path(path)).map(SessionBytes::Mmap)
    }

    /// Whether `session_bytes` is cheap enough to load for every listed
    /// session even when no full-text query needs it.
    fn cheap_session_bytes(&self) -> bool {
        true
    }

    /// Whether every user message from `iter_messages` is a substring of one
    /// JSON string value in `session_bytes`. Prompt-only search then skips
    /// JSON parsing for sessions whose raw bytes cannot contain the query.
    fn prompts_in_session_json(&self) -> bool {
        false
    }

    /// Stronger form of `prompts_in_session_json` for JSONL sessions: each
    /// user message comes from one line, independent of the other lines, so
    /// `iter_messages_from_bytes` yields the same prompts from any subset of
    /// lines that contains theirs. Prompt-only search then parses only the
    /// lines that may match.
    fn prompts_per_jsonl_line(&self) -> bool {
        false
    }

    /// Text searched by the default full-text query: every message from
    /// `iter_messages` plus tool call inputs (tool name, arguments, file
    /// paths) and tool outputs. JSON keys and session metadata are not
    /// included. `visit` returns `false` to stop.
    fn iter_search_texts(&self, path: &Path, visit: &mut dyn FnMut(&str) -> bool) {
        self.iter_messages(path, &mut |message| visit(&message.text));
    }

    /// `iter_search_texts` from `session_bytes` (or a subset of its lines
    /// when `search_texts_per_jsonl_line`), which may differ from the
    /// session file (e.g. Copilot's `events.jsonl`).
    fn iter_search_texts_from_bytes(
        &self,
        path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(&str) -> bool,
    ) {
        self.iter_messages_from_bytes(path, data, &mut |message| visit(&message.text));
    }

    /// `prompts_in_session_json` for every text from `iter_search_texts`:
    /// each is a substring of one JSON string value in `session_bytes`.
    fn search_texts_in_session_json(&self) -> bool {
        false
    }

    /// Cheap byte-level check of one session line when
    /// `search_texts_per_jsonl_line`: `false` only when the line holds no
    /// search text, so it is skipped without decoding or parsing.
    fn line_may_hold_search_texts(&self, _line: &[u8]) -> bool {
        true
    }

    /// `prompts_per_jsonl_line` for every text from `iter_search_texts`:
    /// `iter_search_texts_from_bytes` yields a line's texts from that line
    /// alone.
    fn search_texts_per_jsonl_line(&self) -> bool {
        false
    }

    /// Raw session content for `ah show -f raw`.
    fn raw_content(&self, path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
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

    /// Sessions of this agent that are running now, as `(session id, pid)`.
    /// The id must match what `resolve_resume_id` returns. `pid` is `None`
    /// when the owning process cannot be identified. Only called when
    /// `can_detect_running` is true.
    fn running_sessions(&self) -> Vec<(String, Option<u32>)> {
        Vec::new()
    }

    /// Memory and instruction files that apply to every project
    /// (e.g. `~/.claude/CLAUDE.md`).
    fn global_memory_sources(&self, _home: &Path) -> Vec<MemorySource> {
        Vec::new()
    }

    /// Memory and instruction files the agent reads from a project directory.
    /// Project-level `AGENTS.md` is shared by many agents and listed by
    /// `ah memory` itself, so plugins leave it out.
    fn project_memory_sources(&self, _dir: &Path) -> Vec<MemorySource> {
        Vec::new()
    }

    /// Memory the agent writes itself under its own data directory, keyed by
    /// project (e.g. Claude auto memory). With `cwds`, only files for one of
    /// those project directories and global ones are returned.
    fn agent_memory_files(&self, _home: &Path, _cwds: Option<&[String]>) -> Vec<AgentMemoryFile> {
        Vec::new()
    }
}

/// Map of session id → pid (if known) for every running session of the
/// built-in agents that can detect it.
pub fn running_session_map() -> std::collections::HashMap<String, Option<u32>> {
    all_plugins()
        .iter()
        .filter(|p| p.can_detect_running())
        .flat_map(|p| p.running_sessions())
        .collect()
}

/// How `ah memory` labels a memory/instruction file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// Always-loaded instructions (`CLAUDE.md`, `AGENTS.md`, ...).
    Instruction,
    /// Rule files, often scoped by path or trigger (`.cursor/rules/*.mdc`).
    Rule,
    /// Memory written by the agent; the frontmatter `type` wins when present.
    Memory,
    /// Agent Skills (`SKILL.md`); listed only with `-t skill`.
    Skill,
}

/// A glob of memory/instruction files. `glob` is matched under `base`, which
/// is escaped; an empty `base` means `glob` is already a full pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySource {
    pub base: PathBuf,
    pub glob: String,
    pub kind: MemoryKind,
}

impl MemorySource {
    pub fn new(base: &Path, glob: &str, kind: MemoryKind) -> Self {
        Self {
            base: base.to_path_buf(),
            glob: glob.to_string(),
            kind,
        }
    }

    /// Full glob pattern with the literal base escaped.
    pub fn pattern(&self) -> String {
        if self.base.as_os_str().is_empty() {
            return self.glob.clone();
        }
        let base = glob::Pattern::escape(&self.base.to_string_lossy());
        format!("{}/{}", base.trim_end_matches('/'), self.glob)
    }
}

/// A memory file the agent keeps for a project, with that project's name
/// (`(global)` for memory that applies everywhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMemoryFile {
    pub path: PathBuf,
    pub project: String,
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
static PLUGINS: [&'static dyn AgentPlugin; 8] = [
    &claude::PLUGIN,
    &codex::PLUGIN,
    &gemini::PLUGIN,
    &copilot::PLUGIN,
    &cursor::PLUGIN,
    &agy::PLUGIN,
    &grok::PLUGIN,
    &opencode::PLUGIN,
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
        assert_eq!(find_plugin("agy").unwrap().id(), "agy");
        assert_eq!(find_plugin("grok").unwrap().id(), "grok");
        assert_eq!(find_plugin("opencode").unwrap().id(), "opencode");
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
                "/home/user/.gemini/tmp/proj/chats/session-2026-06-01T00-00-abcd1234.jsonl"
            ))
            .id(),
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
            find_plugin_for_path(Path::new(
                "/home/user/.gemini/antigravity-cli/brain/uuid/.system_generated/logs/transcript.jsonl"
            ))
            .id(),
            "agy"
        );
        assert_eq!(
            find_plugin_for_path(Path::new(
                "/home/user/.grok/sessions/%2Fhome%2Fuser%2Fproj/uuid/chat_history.jsonl"
            ))
            .id(),
            "grok"
        );
        let opencode_db = crate::config::resolve_agent_base("opencode")
            .unwrap()
            .join("opencode/opencode.db/ses_abc");
        assert_eq!(find_plugin_for_path(&opencode_db).id(), "opencode");
        assert_eq!(
            find_plugin_for_path(Path::new("/tmp/random.txt")).id(),
            "unknown"
        );
    }
}

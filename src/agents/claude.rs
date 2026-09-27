use std::path::Path;

use memchr::memmem;

use super::AgentPlugin;
use super::Message;
use super::common::{RE_HOME_PREFIX, for_each_jsonl_value, for_each_jsonl_value_bytes, mmap_file};

pub static PLUGIN: ClaudePlugin = ClaudePlugin;

/// Recent Claude Code versions write several header records (`mode`,
/// `permission-mode`, `bridge-session`, ...) before the first line carrying
/// `cwd`, so look further than the first few lines.
const CWD_SCAN_LINES: usize = 50;

/// How far from the end of a session to look for title records. They are
/// re-appended after every turn and observed within ~40 KiB of the end;
/// a missed title falls back to the first prompt.
const TITLE_SCAN_TAIL_BYTES: usize = 128 * 1024;

pub struct ClaudePlugin;

impl ClaudePlugin {
    fn extract_cwd_from_bytes(data: &[u8]) -> Option<String> {
        let cwd_needle = b"\"cwd\"";
        for line_bytes in data.split(|&b| b == b'\n').take(CWD_SCAN_LINES) {
            if memchr::memmem::find(line_bytes, cwd_needle).is_none() {
                continue;
            }
            let Ok(line) = std::str::from_utf8(line_bytes) else {
                continue;
            };
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                    if !cwd.is_empty() {
                        return Some(cwd.to_string());
                    }
                }
            }
        }
        None
    }

    /// Title priority: user-set custom title, then the title Claude Code
    /// generates, then the first user prompt. Claude Code re-appends title
    /// records after each turn, so the last one of each kind is current and
    /// sits near the end of the file; one backward pass over the tail finds
    /// both.
    fn extract_title_from_bytes(data: &[u8]) -> Option<String> {
        let finder = memmem::FinderRev::new(b"-title\"");
        let tail_start = data.len().saturating_sub(TITLE_SCAN_TAIL_BYTES);
        let mut end = data.len();
        let mut ai_title = None;
        while let Some(rel_pos) = finder.rfind(&data[tail_start..end]) {
            let pos = tail_start + rel_pos;
            let line_start = memchr::memrchr(b'\n', &data[..pos])
                .map(|idx| idx + 1)
                .unwrap_or(0);
            let line_end = memchr::memchr(b'\n', &data[pos..])
                .map(|idx| pos + idx)
                .unwrap_or(data.len());
            // Skip the rest of this line; it cannot hold another record.
            end = line_start.max(tail_start);
            let Ok(val) = serde_json::from_slice::<serde_json::Value>(&data[line_start..line_end])
            else {
                continue;
            };
            let field = match val.get("type").and_then(|v| v.as_str()) {
                Some("custom-title") => "customTitle",
                Some("ai-title") if ai_title.is_none() => "aiTitle",
                _ => continue,
            };
            let Some(title) = val
                .get(field)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if field == "customTitle" {
                return Some(title.to_string());
            }
            ai_title = Some(title.to_string());
        }
        ai_title.or_else(|| Self::first_user_prompt_from_mmap(data))
    }

    pub(crate) fn first_user_prompt_from_mmap(mmap: &[u8]) -> Option<String> {
        for line_bytes in mmap.split(|&b| b == b'\n') {
            if line_bytes.len() < 10 {
                continue;
            }
            if memchr::memmem::find(line_bytes, b"\"type\":\"user\"").is_none() {
                continue;
            }
            let line = std::str::from_utf8(line_bytes).ok()?;
            let val: serde_json::Value = serde_json::from_str(line).ok()?;
            if val.get("type").and_then(|v| v.as_str()) != Some("user") {
                continue;
            }
            let msg = val.get("message")?;
            if let Some(text) = msg
                .get("content")
                .and_then(|v| v.as_str())
                .or_else(|| msg.as_str())
            {
                if !text.starts_with('<') {
                    return Some(text.to_string());
                }
                continue;
            }
            if let Some(contents) = msg.get("content").and_then(|v| v.as_array()) {
                for item in contents {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        if !text.starts_with('<') && !text.starts_with("# ") {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }
        None
    }
}

impl AgentPlugin for ClaudePlugin {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn description(&self) -> &'static str {
        "Claude Code (Anthropic)"
    }

    fn can_resume(&self) -> bool {
        true
    }
    fn can_detect_running(&self) -> bool {
        true
    }
    fn can_memory(&self) -> bool {
        true
    }
    fn can_follow(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: decoded from session dir name)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[".claude/projects/*/*.jsonl"]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.claude/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        for_each_jsonl_value(path, |val| {
            for msg in self.messages_from_value(val) {
                if !visit(msg) {
                    return false;
                }
            }
            true
        });
    }

    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        for_each_jsonl_value_bytes(data, |val| {
            for msg in self.messages_from_value(val) {
                if !visit(msg) {
                    return false;
                }
            }
            true
        });
    }

    fn messages_from_value(&self, val: &serde_json::Value) -> Vec<Message> {
        let mut msgs = Vec::new();
        match val.get("type").and_then(|v| v.as_str()) {
            Some("user") => {
                let Some(msg) = val.get("message") else {
                    return msgs;
                };
                if let Some(text) = msg
                    .get("content")
                    .and_then(|v| v.as_str())
                    .or_else(|| msg.as_str())
                {
                    if !text.starts_with('<') {
                        msgs.push(Message::user(text.to_string()));
                    }
                    return msgs;
                }
                if let Some(contents) = msg.get("content").and_then(|v| v.as_array()) {
                    for item in contents {
                        let text = match item.get("type").and_then(|v| v.as_str()) {
                            Some("text") | Some("input_text") => {
                                item.get("text").and_then(|v| v.as_str())
                            }
                            _ => item.get("text").and_then(|v| v.as_str()),
                        };
                        if let Some(text) = text {
                            if !text.starts_with('<') && !text.starts_with("# ") {
                                msgs.push(Message::user(text.to_string()));
                                return msgs;
                            }
                        }
                    }
                }
            }
            Some("assistant") => {
                if let Some(contents) = val.pointer("/message/content").and_then(|v| v.as_array()) {
                    for item in contents {
                        if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                msgs.push(Message::assistant(text.to_string()));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        msgs
    }

    fn resolve_project(&self, path: &Path, _home: &Path) -> Option<String> {
        let raw = path.parent()?.file_name()?.to_string_lossy();
        Some(RE_HOME_PREFIX.replace(&raw, "").replace('-', "/"))
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        let mmap = mmap_file(path)?;
        Self::extract_cwd_from_bytes(&mmap)
    }

    fn resolve_cwd_from_mmap(&self, _path: &Path, _home: &Path, mmap: &[u8]) -> Option<String> {
        Self::extract_cwd_from_bytes(mmap)
    }

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        let mmap = mmap_file(path)?;
        Self::extract_title_from_bytes(&mmap)
    }

    fn resolve_title_from_mmap(&self, _path: &Path, _home: &Path, mmap: &[u8]) -> Option<String> {
        Self::extract_title_from_bytes(mmap)
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        if path.to_string_lossy().contains("/subagents/") {
            None
        } else {
            path.file_stem().map(|s| s.to_string_lossy().to_string())
        }
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["claude".to_string(), "--resume".to_string(), id])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read(path).unwrap()
    }

    #[test]
    fn cwd_found_after_header_records() {
        let data = fixture("claude_session_headers.jsonl");
        assert_eq!(
            ClaudePlugin::extract_cwd_from_bytes(&data).as_deref(),
            Some("/Users/test/headers-project")
        );
    }

    #[test]
    fn cwd_scan_is_bounded() {
        let mut data = Vec::new();
        for _ in 0..CWD_SCAN_LINES {
            data.extend_from_slice(b"{\"type\":\"mode\",\"mode\":\"default\"}\n");
        }
        data.extend_from_slice(b"{\"type\":\"user\",\"cwd\":\"/late\"}\n");
        assert_eq!(ClaudePlugin::extract_cwd_from_bytes(&data), None);
    }

    #[test]
    fn title_uses_latest_ai_title() {
        let data = fixture("claude_session_headers.jsonl");
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(&data).as_deref(),
            Some("Add uploader retry with logging")
        );
    }

    #[test]
    fn custom_title_wins_over_ai_title_regardless_of_position() {
        let big = "x".repeat(16 * 1024);
        let data = format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"my-name\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{big}\"}}}}\n\
             {{\"type\":\"ai-title\",\"aiTitle\":\"generated\"}}\n"
        );
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(data.as_bytes()).as_deref(),
            Some("my-name")
        );
    }

    #[test]
    fn title_record_type_must_match() {
        let data = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n\
{\"note\":{\"type\":\"ai-title\",\"aiTitle\":\"nested\"},\"type\":\"summary\"}\n";
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(data).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn title_falls_back_to_first_prompt() {
        let data =
            b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"first prompt\"}}\n";
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(data).as_deref(),
            Some("first prompt")
        );
    }
}

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use memchr::memmem;

use super::AgentPlugin;
use super::Message;
use super::common::{RE_HOME_PREFIX, for_each_jsonl_value, for_each_jsonl_value_bytes, mmap_file};

pub static PLUGIN: ClaudePlugin = ClaudePlugin;

pub struct ClaudePlugin;

impl ClaudePlugin {
    fn extract_cwd_from_bytes(data: &[u8]) -> Option<String> {
        let cwd_needle = b"\"cwd\"";
        for (i, line_bytes) in data.split(|&b| b == b'\n').enumerate() {
            if i >= 5 {
                break;
            }
            if memchr::memmem::find(line_bytes, cwd_needle).is_none() {
                continue;
            }
            let line = std::str::from_utf8(line_bytes).ok()?;
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

    fn extract_title_from_bytes(data: &[u8]) -> Option<String> {
        // Try custom-title (appended near end of file).
        // Search only the last 8KB since custom-title is always at the tail.
        let needle = b"\"type\":\"custom-title\"";
        let search_start = data.len().saturating_sub(8192);
        if let Some(rel_pos) = memmem::FinderRev::new(needle).rfind(&data[search_start..]) {
            let pos = search_start + rel_pos;
            let line_start = memchr::memrchr(b'\n', &data[..pos])
                .map(|idx| idx + 1)
                .unwrap_or(0);
            let line_end = memchr::memchr(b'\n', &data[pos..])
                .map(|idx| pos + idx)
                .unwrap_or(data.len());
            if let Ok(line) = std::str::from_utf8(&data[line_start..line_end]) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(title) = val
                        .get("customTitle")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        return Some(title.to_string());
                    }
                }
            }
        }

        // Fallback: extract first user prompt (forward scan, exits early)
        Self::first_user_prompt_from_mmap(data)
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
        let file = fs::File::open(path).ok()?;
        let reader = BufReader::new(file);
        for line in reader.lines().take(5) {
            let line = line.ok()?;
            if line.contains("\"cwd\"") {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                    if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                        if !cwd.is_empty() {
                            return Some(cwd.to_string());
                        }
                    }
                }
            }
        }
        None
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

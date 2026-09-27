use std::path::Path;

use memchr::memmem;

use super::AgentPlugin;
use super::Message;
use super::common::{
    RE_HOME_PREFIX, for_each_jsonl_value, for_each_jsonl_value_bytes, is_pid_alive, json_pid,
    mmap_file, process_start_ticks,
};

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

    /// Value of `field` in the latest `record_type` record within the tail of
    /// the session, or `None` when that record is missing or its value is
    /// empty. Title records always start a line with their `type` key, so
    /// anchoring the needle there never matches message text.
    fn latest_record_value(data: &[u8], record_type: &str, field: &str) -> Option<String> {
        let needle = format!("\n{{\"type\":\"{record_type}\"");
        let finder = memmem::FinderRev::new(needle.as_bytes());
        let tail_start = data.len().saturating_sub(TITLE_SCAN_TAIL_BYTES);
        // Start one byte early so a line beginning exactly at `tail_start`
        // still has its preceding newline inside the searched range.
        let search_start = tail_start.saturating_sub(1);
        let mut end = data.len();
        let mut line_starts = std::iter::from_fn(|| {
            let rel_pos = finder.rfind(&data[search_start..end])?;
            end = search_start + rel_pos;
            Some(end + 1)
        })
        .chain((tail_start == 0 && data.starts_with(&needle.as_bytes()[1..])).then_some(0));
        line_starts.find_map(|line_start| {
            let line_end = memchr::memchr(b'\n', &data[line_start..])
                .map(|idx| line_start + idx)
                .unwrap_or(data.len());
            // A line that fails to parse (e.g. still being appended) is
            // skipped in favor of the previous record.
            let val =
                serde_json::from_slice::<serde_json::Value>(&data[line_start..line_end]).ok()?;
            if val.get("type").and_then(|v| v.as_str()) != Some(record_type) {
                return None;
            }
            Some(
                val.get(field)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string),
            )
        })?
    }

    /// Title priority: user-set custom title, then the title Claude Code
    /// generates, then the first user prompt. Claude Code re-appends title
    /// records after each turn, so the latest one of each kind is current
    /// and sits near the end of the file.
    fn extract_title_from_bytes(data: &[u8]) -> Option<String> {
        Self::latest_record_value(data, "custom-title", "customTitle")
            .or_else(|| Self::latest_record_value(data, "ai-title", "aiTitle"))
            .or_else(|| Self::first_user_prompt_from_mmap(data))
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
    fn prompts_in_session_json(&self) -> bool {
        true
    }
    fn prompts_per_jsonl_line(&self) -> bool {
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

    fn running_sessions(&self) -> Vec<(String, Option<u32>)> {
        crate::config::resolve_agent_base("claude")
            .map(|base| running_in(&base.join("sessions"), process_start_ticks))
            .unwrap_or_default()
    }
}

/// Running sessions from Claude Code's `sessions/<pid>.json` registry.
/// `procStart` records the process start time (on Linux, clock ticks since
/// boot); when both it and the live process's start time are known, a
/// mismatch means the PID was reused by an unrelated process.
fn running_in(
    sessions_dir: &Path,
    start_ticks: impl Fn(u32) -> Option<u64>,
) -> Vec<(String, Option<u32>)> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(val) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        else {
            continue;
        };
        let Some(pid) = json_pid(&val, "pid") else {
            continue;
        };
        let session_id = val.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
        if session_id.is_empty() || !is_pid_alive(pid) {
            continue;
        }
        let recorded = val.get("procStart").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        });
        if let (Some(recorded), Some(actual)) = (recorded, start_ticks(pid)) {
            if recorded != actual {
                continue;
            }
        }
        out.push((session_id.to_string(), Some(pid)));
    }
    out
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

    fn padded_session(title_line: &str, padding: usize) -> Vec<u8> {
        let mut data =
            b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"prompt\"}}\n"
                .to_vec();
        data.extend_from_slice(title_line.as_bytes());
        data.push(b'\n');
        let filler =
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"";
        let fill_len = padding.saturating_sub(filler.len() + "\"}]}}\n".len());
        data.extend_from_slice(filler.as_bytes());
        data.extend(std::iter::repeat_n(b'x', fill_len));
        data.extend_from_slice(b"\"}]}}\n");
        data
    }

    #[test]
    fn title_starting_at_tail_window_start_is_found() {
        let title = "{\"type\":\"custom-title\",\"customTitle\":\"in-window\"}";
        // The title line starts exactly TITLE_SCAN_TAIL_BYTES before the end.
        let data = padded_session(title, TITLE_SCAN_TAIL_BYTES - title.len() - 1);
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(&data).as_deref(),
            Some("in-window")
        );
    }

    #[test]
    fn title_outside_tail_window_falls_back_to_first_prompt() {
        let title = "{\"type\":\"ai-title\",\"aiTitle\":\"too-far\"}";
        // The title line starts one byte before the window.
        let data = padded_session(title, TITLE_SCAN_TAIL_BYTES - title.len());
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(&data).as_deref(),
            Some("prompt")
        );
    }

    #[test]
    fn empty_latest_custom_title_does_not_revive_older_one() {
        let data = b"{\"type\":\"custom-title\",\"customTitle\":\"old\"}\n\
{\"type\":\"ai-title\",\"aiTitle\":\"generated\"}\n\
{\"type\":\"custom-title\",\"customTitle\":\"\"}\n";
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(data).as_deref(),
            Some("generated")
        );
    }

    #[test]
    fn custom_title_keeps_whitespace_but_blank_is_empty() {
        let data = b"{\"type\":\"ai-title\",\"aiTitle\":\"generated\"}\n\
{\"type\":\"custom-title\",\"customTitle\":\" padded \"}\n";
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(data).as_deref(),
            Some(" padded ")
        );
        let blank = b"{\"type\":\"ai-title\",\"aiTitle\":\"generated\"}\n\
{\"type\":\"custom-title\",\"customTitle\":\"   \"}\n";
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(blank).as_deref(),
            Some("generated")
        );
    }

    #[test]
    fn message_text_ending_in_title_is_not_a_record() {
        let data = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n\
{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"x\\n{\\\"type\\\":\\\"ai-title\\\",\\\"aiTitle\\\":\\\"fake\\\"}\"}]}}\n";
        assert_eq!(
            ClaudePlugin::extract_title_from_bytes(data).as_deref(),
            Some("hello")
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

    #[cfg(unix)]
    #[test]
    fn running_in_checks_liveness_and_pid_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let write = |name: &str, body: String| std::fs::write(dir.path().join(name), body).unwrap();
        write(
            "a.json",
            format!(r#"{{"pid":{me},"sessionId":"alive","procStart":"100"}}"#),
        );
        write(
            "b.json",
            format!(r#"{{"pid":{me},"sessionId":"reused","procStart":"999"}}"#),
        );
        write(
            "c.json",
            format!(r#"{{"pid":{me},"sessionId":"no-start"}}"#),
        );
        write(
            "d.json",
            format!(r#"{{"pid":{},"sessionId":"dead"}}"#, i32::MAX),
        );
        write("e.json", format!(r#"{{"pid":{me},"sessionId":""}}"#));
        write(
            "f.txt",
            format!(r#"{{"pid":{me},"sessionId":"not-json-ext"}}"#),
        );

        let mut got = running_in(dir.path(), |pid| (pid == me).then_some(100));
        got.sort();
        assert_eq!(
            got,
            vec![
                ("alive".to_string(), Some(me)),
                ("no-start".to_string(), Some(me)),
            ]
        );

        // Start time unavailable (e.g. non-Linux): fall back to liveness only.
        let mut got = running_in(dir.path(), |_| None);
        got.sort();
        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["alive", "no-start", "reused"]);
    }
}

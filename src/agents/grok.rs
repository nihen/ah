use std::fs;
use std::path::Path;

use super::AgentPlugin;
use super::Message;
use super::common::canonicalize_if_exists;
use super::common::first_text_part;
use super::common::for_each_jsonl_value_bytes;
use super::common::is_pid_alive;
use super::common::is_safe_cli_id;
use super::common::json_pid;
use super::common::mmap_file;
use super::common::percent_decode;
use super::common::strip_home;
use super::common::tagged_user_body;

pub static PLUGIN: GrokPlugin = GrokPlugin;

/// Grok CLI (xAI, "Grok Build").
///
/// Layout: `~/.grok/sessions/<percent-encoded-cwd>/<session-uuid>/chat_history.jsonl`
/// with a sibling `summary.json` holding cwd / title / model metadata.
pub struct GrokPlugin;

impl GrokPlugin {
    /// `<session-dir>/summary.json` parsed as JSON.
    fn read_summary(path: &Path) -> Option<serde_json::Value> {
        let summary = path.parent()?.join("summary.json");
        let content = fs::read_to_string(summary).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Session directory name (UUID).
    fn session_dir_name(path: &Path) -> Option<String> {
        path.parent()?
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
    }

    /// Percent-encoded cwd directory name (grandparent of the JSONL file).
    fn encoded_cwd(path: &Path) -> Option<String> {
        path.parent()?
            .parent()?
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
    }

    fn message_from_value(val: &serde_json::Value) -> Option<Message> {
        match val.get("type").and_then(|v| v.as_str()) {
            Some("user") => {
                // content is an array of blocks: [{"type":"text","text":"<user_query>…</user_query>"}]
                let raw = val.get("content").and_then(first_text_part)?;
                let text = tagged_user_body(raw, "user_query")?;
                Some(Message::user(text.to_string()))
            }
            Some("assistant") => {
                // content is a plain string (may be empty when only tool_calls are present)
                let text = val.get("content").and_then(first_text_part)?;
                if text.trim().is_empty() {
                    return None;
                }
                Some(Message::assistant(text.to_string()))
            }
            _ => None,
        }
    }
}

impl AgentPlugin for GrokPlugin {
    fn id(&self) -> &'static str {
        "grok"
    }

    fn description(&self) -> &'static str {
        "Grok CLI (xAI)"
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

    fn can_follow(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative cwd from summary.json, fallback: percent-encoded cwd dir name)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[".grok/sessions/*/*/chat_history.jsonl"]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.grok/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        if let Some(mmap) = mmap_file(path) {
            self.iter_messages_from_bytes(path, &mmap, visit);
        }
    }

    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        for_each_jsonl_value_bytes(data, |val| match Self::message_from_value(val) {
            Some(msg) => visit(msg),
            None => true,
        });
    }

    fn messages_from_value(&self, val: &serde_json::Value) -> Vec<Message> {
        Self::message_from_value(val).into_iter().collect()
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        if let Some(cwd) = Self::read_summary(path)
            .and_then(|s| {
                s.pointer("/info/cwd")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .filter(|s| !s.is_empty())
        {
            return Some(canonicalize_if_exists(&cwd));
        }
        let encoded = Self::encoded_cwd(path)?;
        let decoded = percent_decode(&encoded);
        if decoded.starts_with('/') {
            Some(canonicalize_if_exists(&decoded))
        } else {
            None
        }
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        // Raw identifier: home-relative cwd (like codex); fall back to the
        // percent-encoded session dir name. Basename is applied by the resolver.
        self.resolve_cwd(path, home)
            .map(|cwd| strip_home(&cwd, home))
            .or_else(|| Self::encoded_cwd(path))
    }

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        let summary = Self::read_summary(path)?;
        for key in ["generated_title", "session_summary"] {
            if let Some(title) = summary.get(key).and_then(|v| v.as_str()) {
                let title = title.trim();
                if !title.is_empty() {
                    return Some(title.to_string());
                }
            }
        }
        None
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        // Prefer summary.json info.id; fall back to the session dir name.
        // Either way the id must be safe to pass as a CLI positional.
        Self::read_summary(path)
            .and_then(|s| s.pointer("/info/id")?.as_str().map(String::from))
            .filter(|id| is_safe_cli_id(id))
            .or_else(|| Self::session_dir_name(path).filter(|id| is_safe_cli_id(id)))
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["grok".to_string(), "--resume".to_string(), id])
    }

    fn running_sessions(&self) -> Vec<(String, Option<u32>)> {
        crate::config::resolve_agent_base("grok")
            .map(|base| running_in(&base.join("active_sessions.json")))
            .unwrap_or_default()
    }
}

/// Running sessions from `active_sessions.json`:
/// `[{"session_id": "...", "pid": N, "cwd": "..."}]`.
fn running_in(active_sessions: &Path) -> Vec<(String, Option<u32>)> {
    let Some(val) = fs::read_to_string(active_sessions)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
    else {
        return Vec::new();
    };
    val.as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let pid = json_pid(entry, "pid")?;
            let id = entry.get("session_id")?.as_str()?;
            (!id.is_empty() && is_pid_alive(pid)).then(|| (id.to_string(), Some(pid)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(dir: &Path, history: &str, summary: Option<&str>) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let jsonl = dir.join("chat_history.jsonl");
        fs::File::create(&jsonl)
            .unwrap()
            .write_all(history.as_bytes())
            .unwrap();
        if let Some(s) = summary {
            fs::write(dir.join("summary.json"), s).unwrap();
        }
        jsonl
    }

    #[test]
    fn parses_user_and_assistant_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join(".grok/sessions/%2Fdata%2Fhome%2Fme%2Fproj/0192-aaaa");
        let history = concat!(
            r#"{"type":"system","content":"You are Grok"}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nhello there\n</user_query>"}],"prompt_index":0}"#,
            "\n",
            r#"{"type":"reasoning","content":null}"#,
            "\n",
            r#"{"type":"assistant","content":"","tool_calls":[{"id":"x","name":"grep","arguments":"{}"}]}"#,
            "\n",
            r#"{"type":"tool_result","tool_call_id":"x","content":"..."}"#,
            "\n",
            r#"{"type":"assistant","content":"hi back","model_id":"grok-4.6-build"}"#,
            "\n",
        );
        let path = write_session(&dir, history, None);
        let mut msgs = Vec::new();
        PLUGIN.iter_messages(&path, &mut |m| {
            msgs.push(m);
            true
        });
        assert_eq!(
            msgs,
            vec![
                Message::user("hello there".to_string()),
                Message::assistant("hi back".to_string()),
            ]
        );
    }

    #[test]
    fn resolves_cwd_title_and_resume_from_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join(".grok/sessions/%2Fdata%2Fhome%2Fme%2Fproj/0192-aaaa");
        let summary = r#"{"info":{"id":"0192-aaaa","cwd":"/data/home/me/proj"},
            "generated_title":"My title","session_summary":"My title","current_model_id":"grok-4.6"}"#;
        let path = write_session(&dir, "", Some(summary));
        let home = tmp.path();
        assert_eq!(
            PLUGIN.resolve_cwd(&path, home).as_deref(),
            Some("/data/home/me/proj")
        );
        assert_eq!(
            PLUGIN.resolve_project(&path, home).as_deref(),
            Some("/data/home/me/proj")
        );
        assert_eq!(
            PLUGIN.resolve_title(&path, home).as_deref(),
            Some("My title")
        );
        assert_eq!(
            PLUGIN.resume_args(&path, home),
            Some(vec![
                "grok".to_string(),
                "--resume".to_string(),
                "0192-aaaa".to_string()
            ])
        );
    }

    #[test]
    fn project_raw_is_home_relative_when_cwd_is_under_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let cwd = home.join("src/proj");
        let dir = home.join(".grok/sessions/enc/0192-cccc");
        let summary = format!(
            r#"{{"info":{{"id":"0192-cccc","cwd":"{}"}}}}"#,
            cwd.to_string_lossy()
        );
        let path = write_session(&dir, "", Some(&summary));
        assert_eq!(
            PLUGIN.resolve_project(&path, home).as_deref(),
            Some("src/proj")
        );
    }

    #[test]
    fn falls_back_to_percent_decoded_dir_without_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join(".grok/sessions/%2Fdata%2Fhome%2Fme%2Fmy-proj/0192-bbbb");
        let path = write_session(&dir, "", None);
        let home = tmp.path();
        assert_eq!(
            PLUGIN.resolve_cwd(&path, home).as_deref(),
            Some("/data/home/me/my-proj")
        );
        assert_eq!(
            PLUGIN.resolve_project(&path, home).as_deref(),
            Some("/data/home/me/my-proj")
        );
        assert_eq!(PLUGIN.resolve_title(&path, home), None);
        assert_eq!(
            PLUGIN.resolve_resume_id(&path, home).as_deref(),
            Some("0192-bbbb")
        );
    }

    #[cfg(unix)]
    #[test]
    fn running_in_keeps_live_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("active_sessions.json");
        let me = std::process::id();
        fs::write(
            &file,
            format!(
                r#"[{{"session_id":"live","pid":{me}}},{{"session_id":"dead","pid":{}}},{{"session_id":"","pid":{me}}},{{"pid":{me}}}]"#,
                i32::MAX
            ),
        )
        .unwrap();
        assert_eq!(running_in(&file), vec![("live".to_string(), Some(me))]);
        assert!(running_in(&dir.path().join("missing.json")).is_empty());
    }
}

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::LazyLock;
use std::time::SystemTime;

use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::for_each_jsonl_value;
use super::common::format_mtime;
use super::common::read_first_line_json;
use super::common::strip_home;

static RE_CODEX_SESSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".*/sessions/(\d+/\d+/\d+)/.*").unwrap());
static RE_CODEX_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"rollout-(\d{4}-\d{2}-\d{2})T(\d{2})-(\d{2})").unwrap());
static RE_CODEX_ROLLOUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^rollout-[\dT-]+-(.+)$").unwrap());

pub static PLUGIN: CodexPlugin = CodexPlugin;

pub struct CodexPlugin;

impl AgentPlugin for CodexPlugin {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn description(&self) -> &'static str {
        "Codex CLI (OpenAI)"
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative path of cwd)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".codex/sessions/**/*.jsonl",
            ".codex/archived_sessions/**/*.jsonl",
        ]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.codex/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        for_each_jsonl_value(path, |val| {
            if val.get("type").and_then(|v| v.as_str()) != Some("response_item") {
                return true;
            }

            match val.pointer("/payload/role").and_then(|v| v.as_str()) {
                Some("user") => {
                    if let Some(contents) =
                        val.pointer("/payload/content").and_then(|v| v.as_array())
                    {
                        for item in contents {
                            let is_user_text = matches!(
                                item.get("type").and_then(|v| v.as_str()),
                                Some("input_text" | "text")
                            );
                            if is_user_text {
                                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                    if !text.starts_with('<')
                                        && !text.starts_with("# ")
                                        && !visit(Message::user(text.to_string()))
                                    {
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                }
                Some("assistant") => {
                    if let Some(contents) =
                        val.pointer("/payload/content").and_then(|v| v.as_array())
                    {
                        for item in contents {
                            if item.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                    if !visit(Message::assistant(text.to_string())) {
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            true
        });
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        if let Some(cwd) = self.resolve_cwd(path, home) {
            Some(strip_home(&cwd, home))
        } else {
            RE_CODEX_SESSIONS
                .captures(&path.to_string_lossy())
                .map(|caps| caps[1].to_string())
                .or_else(|| Some("?".to_string()))
        }
    }

    fn resolve_date(&self, path: &Path, mtime: SystemTime) -> Option<String> {
        RE_CODEX_DATE
            .captures(&path.to_string_lossy())
            .map(|caps| format!("{} {}:{}", &caps[1], &caps[2], &caps[3]))
            .or_else(|| Some(format_mtime(mtime)))
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        let val = read_first_line_json(path)?;
        val.pointer("/payload/cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    fn resolve_title(&self, path: &Path, home: &Path) -> Option<String> {
        let val = read_first_line_json(path)?;
        let session_id = val.pointer("/payload/id")?.as_str()?;
        let index_path = home.join(".codex/session_index.jsonl");
        let index_file = fs::File::open(&index_path).ok()?;
        let index_reader = BufReader::new(index_file);
        for line in index_reader.lines() {
            let line = line.ok()?;
            if line.contains(session_id) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                    if let Some(name) = val.get("thread_name").and_then(|v| v.as_str()) {
                        if !name.is_empty() {
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        if path.to_string_lossy().contains("/archived_sessions/") {
            return None;
        }

        let val = read_first_line_json(path)?;
        if let Some(id) = val.pointer("/payload/id").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }

        let stem = path.file_stem()?.to_string_lossy();
        RE_CODEX_ROLLOUT
            .captures(&stem)
            .map(|caps| caps[1].to_string())
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["codex".to_string(), "resume".to_string(), id])
    }
}

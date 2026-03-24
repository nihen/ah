use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::{for_each_jsonl_value, format_mtime, strip_home};

static RE_COPILOT_SESSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(.*/session-state/[^/]+)/.*").unwrap());

pub static PLUGIN: CopilotPlugin = CopilotPlugin;

pub struct CopilotPlugin;

impl CopilotPlugin {
    fn session_dir(path: &Path) -> Option<PathBuf> {
        RE_COPILOT_SESSION
            .captures(&path.to_string_lossy())
            .map(|caps| PathBuf::from(&caps[1]))
    }

    fn read_workspace_field(path: &Path, field: &str) -> Option<String> {
        let session_dir = Self::session_dir(path)?;
        let content = fs::read_to_string(session_dir.join("workspace.yaml")).ok()?;
        let prefix = format!("{}: ", field);
        content.lines().find_map(|line| {
            line.strip_prefix(&prefix)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
    }
}

impl AgentPlugin for CopilotPlugin {
    fn id(&self) -> &'static str {
        "copilot"
    }

    fn description(&self) -> &'static str {
        "GitHub Copilot CLI"
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative path of cwd)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[".copilot/session-state/*/workspace.yaml"]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.copilot/"]
    }

    fn search_path(&self, path: &Path) -> PathBuf {
        Self::session_dir(path)
            .map(|d| d.join("events.jsonl"))
            .unwrap_or_else(|| path.to_path_buf())
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        let Some(session_dir) = Self::session_dir(path) else {
            return;
        };
        let events_path = session_dir.join("events.jsonl");
        for_each_jsonl_value(&events_path, |val| {
            match val.get("type").and_then(|v| v.as_str()) {
                Some("user.message") => {
                    if let Some(text) = val.pointer("/data/content").and_then(|v| v.as_str()) {
                        return visit(Message::user(text.to_string()));
                    }
                }
                Some("assistant.message") => {
                    if let Some(text) = val.pointer("/data/content").and_then(|v| v.as_str()) {
                        return visit(Message::assistant(text.to_string()));
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
            Self::session_dir(path)
                .and_then(|dir| dir.file_name().map(|f| f.to_string_lossy().to_string()))
                .or_else(|| Some("?".to_string()))
        }
    }

    fn resolve_date(&self, path: &Path, mtime: SystemTime) -> Option<String> {
        Self::read_workspace_field(path, "created_at")
            .and_then(|ts| {
                if ts.len() >= 16 {
                    Some(ts[..16].replace('T', " "))
                } else {
                    None
                }
            })
            .or_else(|| Some(format_mtime(mtime)))
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::read_workspace_field(path, "cwd")
    }

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::read_workspace_field(path, "summary").map(|s| s.trim_matches('"').to_string())
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::session_dir(path).and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["copilot".to_string(), "--resume".to_string(), id])
    }
}

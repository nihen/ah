use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use chrono::DateTime;
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
        yaml_scalar(&content, field).filter(|value| !value.is_empty())
    }

    fn events_path(path: &Path) -> Option<PathBuf> {
        Self::session_dir(path).map(|d| d.join("events.jsonl"))
    }

    fn events_mtime(path: &Path) -> Option<SystemTime> {
        fs::metadata(Self::events_path(path)?)
            .and_then(|m| m.modified())
            .ok()
    }

    /// A `workspace.yaml` timestamp (RFC 3339, written in UTC).
    fn workspace_time(path: &Path, field: &str) -> Option<SystemTime> {
        let ts = Self::read_workspace_field(path, field)?;
        DateTime::parse_from_rfc3339(&ts).ok().map(SystemTime::from)
    }
}

/// Read a top-level scalar from the flat YAML mapping Copilot writes to
/// `workspace.yaml`: plain, single-/double-quoted, or block (`|`, `>`) values.
fn yaml_scalar(content: &str, field: &str) -> Option<String> {
    let mut lines = content.lines();
    let rest = lines.by_ref().find_map(|line| {
        line.strip_prefix(field)
            .and_then(|rest| rest.strip_prefix(':'))
            .filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    })?;
    let rest = rest.trim();
    if rest.starts_with(['|', '>']) {
        let folded = rest.starts_with('>');
        let block: Vec<&str> = lines
            .take_while(|line| line.is_empty() || line.starts_with([' ', '\t']))
            .collect();
        let indent = block
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or(0);
        let body: Vec<&str> = block
            .iter()
            .map(|line| line.get(indent..).unwrap_or("").trim_end())
            .collect();
        return Some(
            body.join(if folded { " " } else { "\n" })
                .trim()
                .to_string(),
        );
    }
    if let Some(quoted) = rest.strip_prefix('\'') {
        return Some(unquote(quoted, &mut lines, false));
    }
    if let Some(quoted) = rest.strip_prefix('"') {
        return Some(unquote(quoted, &mut lines, true));
    }
    Some(rest.to_string())
}

/// Body of a quoted scalar: `''` escapes a quote in single-quoted style,
/// backslash escapes apply in double-quoted style. A scalar that continues
/// onto following lines is folded with spaces.
fn unquote<'a>(first: &'a str, rest: &mut impl Iterator<Item = &'a str>, double: bool) -> String {
    let quote = if double { '"' } else { '\'' };
    let mut out = String::new();
    let mut line = first;
    loop {
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == quote {
                if !double && chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                    continue;
                }
                return out;
            }
            if double && c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        if let Some(ch) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            out.push(ch);
                        }
                    }
                    Some(other) => out.push(other),
                    None => {}
                }
                continue;
            }
            out.push(c);
        }
        match rest.next() {
            Some(next) => {
                out.push(' ');
                line = next.trim_start();
            }
            None => return out,
        }
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
        Self::events_path(path).unwrap_or_else(|| path.to_path_buf())
    }

    /// The session's last activity: `events.jsonl` keeps growing after
    /// `workspace.yaml` stops changing.
    fn session_mtime(&self, path: &Path) -> Option<SystemTime> {
        let yaml = fs::metadata(path).and_then(|m| m.modified()).ok()?;
        Some(Self::events_mtime(path).map_or(yaml, |events| events.max(yaml)))
    }

    fn session_created(&self, path: &Path) -> Option<SystemTime> {
        Self::workspace_time(path, "created_at")
            .or_else(|| fs::metadata(path).and_then(|m| m.created()).ok())
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        let Some(events_path) = Self::events_path(path) else {
            return;
        };
        for_each_jsonl_value(&events_path, |val| {
            // Tool-call-only assistant turns carry an empty `content`.
            let text = val
                .pointer("/data/content")
                .and_then(|v| v.as_str())
                .filter(|text| !text.trim().is_empty());
            match (val.get("type").and_then(|v| v.as_str()), text) {
                (Some("user.message"), Some(text)) => visit(Message::user(text.to_string())),
                (Some("assistant.message"), Some(text)) => {
                    visit(Message::assistant(text.to_string()))
                }
                _ => true,
            }
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
        let modified = self
            .session_mtime(path)
            .or_else(|| Self::workspace_time(path, "updated_at"))
            .unwrap_or(mtime);
        Some(format_mtime(modified))
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::read_workspace_field(path, "cwd")
    }

    /// `name` (set by Copilot or `/rename`) first, then the older `summary`.
    /// A session that never got an `events.jsonl` has no prompt to fall back
    /// on, so it is titled by its id rather than the file name `workspace`.
    fn resolve_title(&self, path: &Path, home: &Path) -> Option<String> {
        ["name", "summary"]
            .iter()
            .find_map(|field| {
                let value = Self::read_workspace_field(path, field)?;
                let first_line = value.lines().map(str::trim).find(|l| !l.is_empty())?;
                Some(first_line.to_string())
            })
            .or_else(|| {
                if Self::events_path(path)?.exists() {
                    None
                } else {
                    self.resolve_resume_id(path, home)
                }
            })
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::session_dir(path).and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        // `--resume` takes an optional value; the attached form is the one
        // `copilot --help` documents for a specific session.
        Some(vec!["copilot".to_string(), format!("--resume={}", id)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str = "id: s1\n\
cwd: /work/proj\n\
name: 'It''s a \"name\"'\n\
summary: |-\n  first line\n  second line\n\
quoted: \"a\\tb \\u00e9\"\n\
folded: >\n  one\n  two\n\
wrapped: 'long\n  value'\n\
plain: list models\n\
created_at: 2026-09-27T02:34:43.613Z\n";

    #[test]
    fn yaml_scalar_styles() {
        assert_eq!(yaml_scalar(WORKSPACE, "name").unwrap(), "It's a \"name\"");
        assert_eq!(
            yaml_scalar(WORKSPACE, "summary").unwrap(),
            "first line\nsecond line"
        );
        assert_eq!(yaml_scalar(WORKSPACE, "quoted").unwrap(), "a\tb é");
        assert_eq!(yaml_scalar(WORKSPACE, "folded").unwrap(), "one two");
        assert_eq!(yaml_scalar(WORKSPACE, "wrapped").unwrap(), "long value");
        assert_eq!(yaml_scalar(WORKSPACE, "plain").unwrap(), "list models");
        // A key that is only a prefix of another key does not match.
        assert_eq!(yaml_scalar(WORKSPACE, "creat"), None);
        assert_eq!(yaml_scalar(WORKSPACE, "missing"), None);
    }

    fn session(dir: &Path, id: &str, yaml: &str, events: Option<&str>) -> PathBuf {
        let session_dir = dir.join(".copilot/session-state").join(id);
        fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("workspace.yaml");
        fs::write(&path, yaml).unwrap();
        if let Some(events) = events {
            fs::write(session_dir.join("events.jsonl"), events).unwrap();
        }
        path
    }

    #[test]
    fn title_prefers_name_then_summary_first_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let named = session(home, "a", WORKSPACE, Some(""));
        assert_eq!(
            PLUGIN.resolve_title(&named, home).unwrap(),
            "It's a \"name\""
        );
        let summary_only = session(home, "b", "summary: |-\n  first\n  second\n", Some(""));
        assert_eq!(PLUGIN.resolve_title(&summary_only, home).unwrap(), "first");
        // With events but no name/summary, the first prompt is used instead.
        let untitled = session(home, "c", "cwd: /x\n", Some(""));
        assert_eq!(PLUGIN.resolve_title(&untitled, home), None);
        // Without events there is no prompt either; the id stands in.
        let empty = session(home, "d-uuid", "cwd: /x\n", None);
        assert_eq!(PLUGIN.resolve_title(&empty, home).unwrap(), "d-uuid");
    }

    #[test]
    fn skips_empty_assistant_turns() {
        let tmp = tempfile::TempDir::new().unwrap();
        let events = concat!(
            r#"{"type":"user.message","data":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"","toolRequests":[{}]}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"hello"}}"#,
            "\n"
        );
        let path = session(tmp.path(), "e", "cwd: /x\n", Some(events));
        let mut texts = Vec::new();
        PLUGIN.iter_messages(&path, &mut |m| {
            texts.push((m.role, m.text));
            true
        });
        assert_eq!(
            texts,
            vec![
                (super::super::MessageRole::User, "hi".to_string()),
                (super::super::MessageRole::Assistant, "hello".to_string()),
            ]
        );
    }

    #[test]
    fn created_at_is_parsed_as_utc() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = session(tmp.path(), "f", WORKSPACE, None);
        let created = PLUGIN.session_created(&path).unwrap();
        let expected: SystemTime = DateTime::parse_from_rfc3339("2026-09-27T02:34:43.613+00:00")
            .unwrap()
            .into();
        assert_eq!(created, expected);
    }

    #[test]
    fn resume_uses_attached_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = session(tmp.path(), "abc-123", WORKSPACE, None);
        assert_eq!(
            PLUGIN.resume_args(&path, tmp.path()).unwrap(),
            vec!["copilot".to_string(), "--resume=abc-123".to_string()]
        );
    }
}

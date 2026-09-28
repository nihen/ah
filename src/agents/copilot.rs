use super::entries::{RecordVisitor, SearchKind as K, TypedVisitor};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use chrono::DateTime;
use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::{
    for_each_jsonl_value, for_each_jsonl_value_bytes, format_mtime, is_pid_alive,
    process_start_time, strip_home, visit_tool_call, visit_tool_output,
};
use super::{MemoryKind, MemorySource};

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

    fn workspace_path(path: &Path) -> PathBuf {
        Self::session_dir(path)
            .map(|d| d.join("workspace.yaml"))
            .unwrap_or_else(|| path.to_path_buf())
    }

    /// A `workspace.yaml` timestamp (RFC 3339, written in UTC).
    fn workspace_time(path: &Path, field: &str) -> Option<SystemTime> {
        let ts = Self::read_workspace_field(path, field)?;
        DateTime::parse_from_rfc3339(&ts).ok().map(SystemTime::from)
    }
}

/// Whether a line continues the current value (indented or blank) rather
/// than starting the next top-level key.
fn is_continuation(line: &str) -> bool {
    line.trim().is_empty() || line.starts_with([' ', '\t'])
}

/// Read a top-level scalar from the flat YAML mapping Copilot writes to
/// `workspace.yaml`: plain, single-/double-quoted, or block (`|`, `>`) values.
/// A plain `null` / `~` and a malformed quoted value count as absent.
fn yaml_scalar(content: &str, field: &str) -> Option<String> {
    let mut lines = content.lines();
    let rest = lines.by_ref().find_map(|line| {
        line.strip_prefix(field)
            .and_then(|rest| rest.strip_prefix(':'))
            .filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    })?;
    let rest = rest.trim();
    let mut continuation = lines.take_while(|line| is_continuation(line));
    if rest.starts_with(['|', '>']) {
        let folded = rest.starts_with('>');
        let block: Vec<&str> = continuation.collect();
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
        let text = if folded {
            // Folded style joins lines with spaces; a blank line is a newline.
            body.split(|line| line.is_empty())
                .map(|para| para.join(" "))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            body.join("\n")
        };
        return Some(text.trim().to_string());
    }
    if let Some(quoted) = rest.strip_prefix('\'') {
        return unquote(quoted, &mut continuation, false);
    }
    if let Some(quoted) = rest.strip_prefix('"') {
        return unquote(quoted, &mut continuation, true);
    }
    if matches!(rest, "null" | "Null" | "NULL" | "~") {
        return None;
    }
    Some(rest.to_string())
}

/// Body of a quoted scalar: `''` escapes a quote in single-quoted style,
/// backslash escapes apply in double-quoted style. A scalar that continues
/// onto following (indented) lines is folded with spaces. `None` when the
/// closing quote is missing.
fn unquote<'a>(
    first: &'a str,
    rest: &mut impl Iterator<Item = &'a str>,
    double: bool,
) -> Option<String> {
    let quote = if double { '"' } else { '\'' };
    let mut out = String::new();
    let mut line = first;
    loop {
        let mut chars = line.chars().peekable();
        // A `\` at the end of a double-quoted line joins the next line
        // without the folding space.
        let mut escaped_break = false;
        while let Some(c) = chars.next() {
            if c == quote {
                if !double && chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                    continue;
                }
                return Some(out);
            }
            if double && c == '\\' {
                if chars.peek().is_none() {
                    escaped_break = true;
                } else {
                    push_escape(&mut out, &mut chars)?;
                }
                continue;
            }
            out.push(c);
        }
        // Line folding: blank lines become newlines; otherwise a single break
        // becomes a space, or nothing after an escaped break.
        let mut blank_lines = 0;
        line = loop {
            let next = rest.next()?;
            if next.trim().is_empty() {
                blank_lines += 1;
            } else {
                break next.trim_start();
            }
        };
        if !escaped_break {
            out.truncate(out.trim_end_matches([' ', '\t']).len());
        }
        if blank_lines > 0 {
            out.extend(std::iter::repeat_n('\n', blank_lines));
        } else if !escaped_break {
            out.push(' ');
        }
    }
}

/// Decode one YAML double-quoted escape (the part after `\`).
fn push_escape(out: &mut String, chars: &mut impl Iterator<Item = char>) -> Option<()> {
    let hex = |chars: &mut dyn Iterator<Item = char>, len: usize| {
        let digits: String = chars.take(len).collect();
        u32::from_str_radix(&digits, 16)
            .ok()
            .and_then(char::from_u32)
    };
    let decoded = match chars.next()? {
        '0' => '\0',
        'a' => '\x07',
        'b' => '\x08',
        't' | '\t' => '\t',
        'n' => '\n',
        'v' => '\x0b',
        'f' => '\x0c',
        'r' => '\r',
        'e' => '\x1b',
        ' ' => ' ',
        'N' => '\u{85}',
        '_' => '\u{a0}',
        'L' => '\u{2028}',
        'P' => '\u{2029}',
        'x' => hex(chars, 2)?,
        'u' => hex(chars, 4)?,
        'U' => hex(chars, 8)?,
        other => other,
    };
    out.push(decoded);
    Some(())
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
    fn prompts_in_session_json(&self) -> bool {
        true
    }
    fn search_texts_in_session_json(&self) -> bool {
        true
    }
    fn search_texts_per_jsonl_line(&self) -> bool {
        true
    }

    fn can_detect_running(&self) -> bool {
        // Needs a pid liveness check, which is only implemented on Unix.
        cfg!(unix)
    }

    fn can_memory(&self) -> bool {
        true
    }

    fn global_memory_sources(&self, home: &Path) -> Vec<MemorySource> {
        let base =
            crate::config::resolve_agent_base(self.id()).unwrap_or_else(|| home.join(".copilot"));
        vec![MemorySource::new(
            &base,
            "copilot-instructions.md",
            MemoryKind::Instruction,
        )]
    }

    fn project_memory_sources(&self, dir: &Path) -> Vec<MemorySource> {
        vec![
            MemorySource::new(
                dir,
                ".github/copilot-instructions.md",
                MemoryKind::Instruction,
            ),
            // Path-specific instructions (`applyTo` frontmatter).
            MemorySource::new(
                dir,
                ".github/instructions/**/*.instructions.md",
                MemoryKind::Rule,
            ),
        ]
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
        let mtime = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
        let yaml = mtime(&Self::workspace_path(path));
        let events = Self::events_path(path).and_then(|p| mtime(&p));
        yaml.max(events).or_else(|| mtime(path))
    }

    /// Size of the session data: `events.jsonl` holds the conversation,
    /// `workspace.yaml` only metadata.
    fn session_size(&self, path: &Path) -> Option<u64> {
        let size = |p: &Path| fs::metadata(p).map(|m| m.len()).ok();
        Self::events_path(path)
            .and_then(|p| size(&p))
            .or_else(|| size(path))
    }

    fn session_created(&self, path: &Path) -> Option<SystemTime> {
        Self::workspace_time(path, "created_at").or_else(|| {
            fs::metadata(Self::workspace_path(path))
                .and_then(|m| m.created())
                .ok()
        })
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

    /// Messages plus tool calls (`tool.execution_start`: tool name and
    /// arguments) and tool results (`tool.execution_complete`).
    fn iter_search_records(&self, path: &Path, visit: &mut RecordVisitor<'_>) {
        if let Some(data) = self.session_bytes(path) {
            for_each_jsonl_value_bytes(&data, |val| {
                visit.record(|emit| visit_search_record(val, emit))
            });
        }
    }

    fn iter_search_texts(&self, path: &Path, visit: &mut dyn FnMut(&str) -> bool) {
        if let Some(bytes) = self.session_bytes(path) {
            self.iter_search_texts_from_bytes(path, &bytes, visit);
        }
    }

    /// `data` is `session_bytes`, i.e. `events.jsonl`.
    fn iter_search_texts_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(&str) -> bool,
    ) {
        for_each_jsonl_value_bytes(data, |val| {
            visit_search_record(val, &mut |_, text| visit(text))
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
    /// Without either, the first prompt is used (resolver fallback); a
    /// session with no prompt at all (e.g. no `events.jsonl`) is titled by its
    /// id rather than the file name `workspace`.
    fn resolve_title(&self, path: &Path, home: &Path) -> Option<String> {
        ["name", "summary"]
            .iter()
            .find_map(|field| {
                let value = Self::read_workspace_field(path, field)?;
                let first_line = value.lines().map(str::trim).find(|l| !l.is_empty())?;
                Some(first_line.to_string())
            })
            .or_else(|| {
                let mut has_prompt = false;
                self.iter_messages(path, &mut |message| {
                    has_prompt = message.role == super::MessageRole::User;
                    !has_prompt
                });
                if has_prompt {
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

    fn running_sessions(&self) -> Vec<(String, Option<u32>)> {
        crate::config::resolve_agent_base("copilot")
            .map(|base| running_in(&base.join("session-state"), process_start_time))
            .unwrap_or_default()
    }
}

/// Slack for comparing a lock file's mtime with its owner's start time.
const LOCK_START_SLACK: Duration = Duration::from_secs(2);

/// Running sessions from the `session-state/<id>/inuse.<pid>.lock` files an
/// interactive Copilot CLI holds while a session is open. Locks survive a
/// crash, so the pid must be alive, and a process that started after the
/// lock was written is an unrelated one that reused the pid.
fn running_in(
    state_dir: &Path,
    start_time: impl Fn(u32) -> Option<SystemTime>,
) -> Vec<(String, Option<u32>)> {
    let Ok(sessions) = fs::read_dir(state_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for session in sessions.flatten() {
        let Ok(files) = fs::read_dir(session.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_prefix("inuse.")?.strip_suffix(".lock"))
                .and_then(|p| p.parse::<u32>().ok())
            else {
                continue;
            };
            if !is_pid_alive(pid) {
                continue;
            }
            let locked_at = file.metadata().and_then(|m| m.modified()).ok();
            if let (Some(locked_at), Some(started)) = (locked_at, start_time(pid)) {
                if started > locked_at + LOCK_START_SLACK {
                    continue;
                }
            }
            out.push((
                session.file_name().to_string_lossy().into_owned(),
                Some(pid),
            ));
            break;
        }
    }
    out
}

fn visit_search_record(val: &serde_json::Value, visit: &mut TypedVisitor<'_>) -> bool {
    let data = val.get("data");
    match val.get("type").and_then(|v| v.as_str()) {
        Some("user.message" | "assistant.message") => data
            .and_then(|d| d.get("content"))
            .and_then(|v| v.as_str())
            .filter(|text| !text.trim().is_empty())
            .is_none_or(|text| {
                visit(
                    if val.get("type").and_then(|v| v.as_str()) == Some("user.message") {
                        K::User
                    } else {
                        K::Assistant
                    },
                    text,
                )
            }),
        Some("tool.execution_start") => data.is_none_or(|d| {
            visit_tool_call(d.get("toolName"), d.get("arguments"), &mut |text| {
                visit(K::ToolInput, text)
            })
        }),
        Some("tool.execution_complete") => data
            .and_then(|d| d.get("result"))
            .is_none_or(|v| visit_tool_output(v, &mut |text| visit(K::ToolOutput, text))),
        _ => true,
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

    #[test]
    fn yaml_scalar_edge_cases() {
        let yaml = "esc: \"cr\\rX \\x41 \\U0001F600 nul\\0 q\\\" b\\\\\"\n\
null_name: null\n\
tilde: ~\n\
quoted_null: 'null'\n\
broken: 'abc def\n\
next: value\n\
joined: \"long\\\n  title\"\n\
joined_blank: \"long\\\n\n  title\"\n\
folded_quote: 'one  \n  two\n\n  three'\n\
paras: >\n  one\n  two\n\n  three\n";
        assert_eq!(
            yaml_scalar(yaml, "esc").unwrap(),
            "cr\rX A \u{1F600} nul\0 q\" b\\"
        );
        assert_eq!(yaml_scalar(yaml, "null_name"), None);
        assert_eq!(yaml_scalar(yaml, "tilde"), None);
        assert_eq!(yaml_scalar(yaml, "quoted_null").unwrap(), "null");
        // A missing closing quote does not swallow the next key.
        assert_eq!(yaml_scalar(yaml, "broken"), None);
        assert_eq!(yaml_scalar(yaml, "next").unwrap(), "value");
        assert_eq!(yaml_scalar(yaml, "joined").unwrap(), "longtitle");
        assert_eq!(yaml_scalar(yaml, "joined_blank").unwrap(), "long\ntitle");
        assert_eq!(yaml_scalar(yaml, "folded_quote").unwrap(), "one two\nthree");
        assert_eq!(yaml_scalar(yaml, "paras").unwrap(), "one two\nthree");
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
        // A null name does not hide the summary.
        let null_name = session(home, "b2", "name: null\nsummary: useful\n", Some(""));
        assert_eq!(PLUGIN.resolve_title(&null_name, home).unwrap(), "useful");
        // With a prompt but no name/summary, the resolver's first-prompt
        // fallback is used instead.
        let prompt = r#"{"type":"user.message","data":{"content":"hi"}}"#;
        let untitled = session(home, "c", "cwd: /x\n", Some(prompt));
        assert_eq!(PLUGIN.resolve_title(&untitled, home), None);
        // Without any prompt (empty or missing events) the id stands in.
        let no_prompt = session(home, "c-uuid", "cwd: /x\n", Some(""));
        assert_eq!(PLUGIN.resolve_title(&no_prompt, home).unwrap(), "c-uuid");
        let empty = session(home, "d-uuid", "cwd: /x\n", None);
        assert_eq!(PLUGIN.resolve_title(&empty, home).unwrap(), "d-uuid");
    }

    #[test]
    fn modified_is_the_later_of_workspace_and_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let new = old + std::time::Duration::from_secs(3600);
        let set = |p: &Path, t: SystemTime| {
            fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t)
                .unwrap()
        };

        let path = session(tmp.path(), "m", "cwd: /x\n", Some(""));
        let events = path.with_file_name("events.jsonl");
        set(&path, old);
        set(&events, new);
        assert_eq!(PLUGIN.session_mtime(&path), Some(new));
        assert_eq!(PLUGIN.resolve_date(&path, old).unwrap(), format_mtime(new));
        // Addressed through events.jsonl, workspace.yaml still counts.
        set(&path, new);
        set(&events, old);
        assert_eq!(PLUGIN.session_mtime(&events), Some(new));

        let yaml_only = session(tmp.path(), "n", "cwd: /x\n", None);
        set(&yaml_only, old);
        assert_eq!(PLUGIN.session_mtime(&yaml_only), Some(old));
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

    #[cfg(unix)]
    #[test]
    fn running_in_requires_live_owner_that_predates_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let lock = |session: &str, pid: u32| {
            let d = dir.path().join(session);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(format!("inuse.{pid}.lock")), pid.to_string()).unwrap();
        };
        lock("live", me);
        lock("dead", i32::MAX as u32);
        fs::create_dir_all(dir.path().join("idle")).unwrap();
        fs::write(dir.path().join("idle/workspace.yaml"), "id: idle").unwrap();
        fs::write(dir.path().join("idle/inuse.notapid.lock"), "").unwrap();

        let past = SystemTime::now() - Duration::from_secs(3600);
        assert_eq!(
            running_in(dir.path(), |_| Some(past)),
            vec![("live".to_string(), Some(me))]
        );
        // Unknown start time: liveness only.
        assert_eq!(
            running_in(dir.path(), |_| None),
            vec![("live".to_string(), Some(me))]
        );
        // Owner started after the lock was written: the pid was reused.
        let future = SystemTime::now() + Duration::from_secs(3600);
        assert!(running_in(dir.path(), |_| Some(future)).is_empty());
        assert!(running_in(&dir.path().join("missing"), |_| None).is_empty());
    }
}

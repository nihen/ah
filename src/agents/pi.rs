use super::entries::{RecordVisitor, SearchKind as K, TypedVisitor};
use std::path::{Path, PathBuf};

use memchr::memmem;

use super::common::{
    for_each_jsonl_value, for_each_jsonl_value_bytes, is_safe_cli_id, mmap_file, strip_home,
    visit_tool_call, visit_tool_output,
};
use super::{AgentPlugin, MemoryKind, MemorySource, Message};

pub static PLUGIN: PiPlugin = PiPlugin;

/// Pi coding agent.
///
/// Layout: `~/.pi/agent/sessions/--<cwd>--/<timestamp>_<session id>.jsonl`.
/// The first line is the session header (`id`, `cwd`); every other line is
/// an entry with an ISO `timestamp`. Entries form a tree (`id`/`parentId`),
/// so one file holds every branch of a session; all of them are read, in
/// file order.
pub struct PiPlugin;

/// Text parts of a message body: a string, or the `text` parts of an array.
fn text_parts(content: &serde_json::Value) -> impl Iterator<Item = &str> {
    let whole = content.as_str();
    let parts = content.as_array().into_iter().flatten().filter_map(|part| {
        (part.get("type").and_then(|v| v.as_str()) == Some("text"))
            .then(|| part.get("text").and_then(|v| v.as_str()))
            .flatten()
    });
    whole.into_iter().chain(parts)
}

/// The message of a `message` entry with its role.
fn entry_message(val: &serde_json::Value) -> Option<(&str, &serde_json::Value)> {
    if val.get("type")?.as_str()? != "message" {
        return None;
    }
    let message = val.get("message")?;
    Some((message.get("role")?.as_str()?, message))
}

impl PiPlugin {
    /// The session header is the first line of the file.
    fn header(data: &[u8]) -> Option<serde_json::Value> {
        let line = data.split(|&b| b == b'\n').next()?;
        let val: serde_json::Value = serde_json::from_slice(line).ok()?;
        (val.get("type")?.as_str()? == "session").then_some(val)
    }

    fn cwd_from_bytes(data: &[u8]) -> Option<String> {
        let header = Self::header(data)?;
        let cwd = header.get("cwd")?.as_str()?;
        (!cwd.is_empty()).then(|| cwd.to_string())
    }

    /// The name set with `/name` or `--name` (the latest `session_info`
    /// entry), or the first prompt.
    fn title_from_bytes(data: &[u8]) -> Option<String> {
        let finder = memmem::Finder::new(b"\"session_info\"");
        let mut name = None;
        for line in data.split(|&b| b == b'\n') {
            if finder.find(line).is_none() {
                continue;
            }
            let Ok(val) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            if val.get("type").and_then(|v| v.as_str()) != Some("session_info") {
                continue;
            }
            // An empty name clears an earlier one.
            name = val
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
        }
        name.or_else(|| {
            let mut first = None;
            for_each_jsonl_value_bytes(data, |val| {
                first = PLUGIN
                    .messages_from_value(val)
                    .into_iter()
                    .find_map(|m| (m.role == super::MessageRole::User).then_some(m.text));
                first.is_none()
            });
            first
        })
    }

    /// Session id from the file name (`<timestamp>_<id>.jsonl`; the
    /// timestamp has no `_`).
    fn id_from_name(path: &Path) -> Option<String> {
        let stem = path.file_stem()?.to_str()?;
        let (_, id) = stem.split_once('_')?;
        (!id.is_empty()).then(|| id.to_string())
    }

    /// Search texts of one entry: message texts, tool calls and tool
    /// results, and commands run with `!` with their output. Returns
    /// `false` when `visit` stops.
    fn visit_search_texts(val: &serde_json::Value, visit: &mut TypedVisitor<'_>) -> bool {
        let Some((role, message)) = entry_message(val) else {
            return true;
        };
        let content = message.get("content").unwrap_or(&serde_json::Value::Null);
        match role {
            "user" => text_parts(content).all(|text| visit(K::User, text)),
            "assistant" => {
                text_parts(content).all(|text| visit(K::Assistant, text))
                    && content.as_array().is_none_or(|parts| {
                        parts
                            .iter()
                            .filter(|part| {
                                part.get("type").and_then(|v| v.as_str()) == Some("toolCall")
                            })
                            .all(|part| {
                                visit_tool_call(
                                    part.get("name"),
                                    part.get("arguments"),
                                    &mut |text| visit(K::ToolInput, text),
                                )
                            })
                    })
            }
            "toolResult" => visit_tool_output(content, &mut |text| visit(K::ToolOutput, text)),
            "bashExecution" => {
                let text = |key| {
                    message
                        .get(key)
                        .and_then(|v: &serde_json::Value| v.as_str())
                };
                text("command").is_none_or(|command| visit(K::ToolInput, command))
                    && text("output").is_none_or(|output| visit(K::ToolOutput, output))
            }
            _ => true,
        }
    }
}

impl AgentPlugin for PiPlugin {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn description(&self) -> &'static str {
        "Pi coding agent"
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
    fn search_texts_in_session_json(&self) -> bool {
        true
    }
    fn search_texts_per_jsonl_line(&self) -> bool {
        true
    }
    fn can_memory(&self) -> bool {
        true
    }
    fn can_follow(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative cwd from the session header, fallback: session dir name)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[".pi/agent/sessions/*/*.jsonl"]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.pi/agent/"]
    }

    /// Only `message` entries hold search texts. The needle is matched
    /// loosely, so a line is skipped only when it cannot be one.
    fn line_may_hold_search_texts(&self, line: &[u8]) -> bool {
        memmem::find(line, b"\"message\"").is_some()
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        for_each_jsonl_value(path, |val| {
            self.messages_from_value(val).into_iter().all(&mut *visit)
        });
    }

    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        for_each_jsonl_value_bytes(data, |val| {
            self.messages_from_value(val).into_iter().all(&mut *visit)
        });
    }

    /// A user entry is one prompt (its first text part, so that the prompt
    /// is one JSON string of the line); an assistant entry gives a message
    /// per text part.
    fn messages_from_value(&self, val: &serde_json::Value) -> Vec<Message> {
        let Some((role, message)) = entry_message(val) else {
            return Vec::new();
        };
        let content = message.get("content").unwrap_or(&serde_json::Value::Null);
        let time = val.get("timestamp");
        let texts = text_parts(content).filter(|text| !text.trim().is_empty());
        match role {
            "user" => texts
                .take(1)
                .map(|text| Message::user(text.to_string()).at(time))
                .collect(),
            "assistant" => texts
                .map(|text| Message::assistant(text.to_string()).at(time))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn iter_search_records(&self, path: &Path, visit: &mut RecordVisitor<'_>) {
        for_each_jsonl_value(path, |val| {
            visit.record(|emit| Self::visit_search_texts(val, emit))
        });
    }

    fn iter_search_texts(&self, path: &Path, visit: &mut dyn FnMut(&str) -> bool) {
        for_each_jsonl_value(path, |val| {
            Self::visit_search_texts(val, &mut |_, text| visit(text))
        });
    }

    fn iter_search_texts_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(&str) -> bool,
    ) {
        for_each_jsonl_value_bytes(data, |val| {
            Self::visit_search_texts(val, &mut |_, text| visit(text))
        });
    }

    /// The model each assistant message names. A message without content
    /// (a request that failed) did not answer.
    fn resolve_models(&self, path: &Path) -> Option<super::common::ModelList> {
        super::common::jsonl_models(path, b"\"model\"", |val| {
            let (role, message) = entry_message(val)?;
            let answered = message
                .get("content")
                .and_then(|v| v.as_array())
                .is_some_and(|parts| !parts.is_empty());
            (role == "assistant" && answered)
                .then(|| message.get("model")?.as_str())
                .flatten()
                .filter(|model| !model.is_empty())
        })
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        self.resolve_cwd(path, home)
            .map(|cwd| strip_home(&cwd, home))
            .or_else(|| {
                let dir = path.parent()?.file_name()?.to_str()?;
                Some(dir.trim_matches('-').to_string())
            })
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::cwd_from_bytes(&mmap_file(path)?)
    }

    fn resolve_cwd_from_mmap(&self, _path: &Path, _home: &Path, mmap: &[u8]) -> Option<String> {
        Self::cwd_from_bytes(mmap)
    }

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::title_from_bytes(&mmap_file(path)?)
    }

    fn resolve_title_from_mmap(&self, _path: &Path, _home: &Path, mmap: &[u8]) -> Option<String> {
        Self::title_from_bytes(mmap)
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::id_from_name(path)
            .or_else(|| {
                let header = Self::header(&mmap_file(path)?)?;
                Some(header.get("id")?.as_str()?.to_string())
            })
            .filter(|id| is_safe_cli_id(id))
    }

    /// The file is passed rather than the id: Pi looks an id up in the
    /// current directory's sessions first and offers to fork a session of
    /// another directory.
    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        self.resolve_resume_id(path, home)?;
        Some(vec![
            "pi".to_string(),
            "--session".to_string(),
            path.to_string_lossy().to_string(),
        ])
    }

    fn global_memory_sources(&self, home: &Path) -> Vec<MemorySource> {
        let base = pi_base(home);
        vec![
            MemorySource::new(&base, "AGENTS.override.md", MemoryKind::Instruction),
            MemorySource::new(&base, "AGENTS.md", MemoryKind::Instruction),
            MemorySource::new(&base, "CLAUDE.md", MemoryKind::Instruction),
            MemorySource::new(&base, "SYSTEM.md", MemoryKind::Instruction),
            MemorySource::new(&base, "APPEND_SYSTEM.md", MemoryKind::Instruction),
            MemorySource::new(&base, "skills/**/SKILL.md", MemoryKind::Skill),
        ]
    }

    fn project_memory_sources(&self, dir: &Path) -> Vec<MemorySource> {
        vec![
            MemorySource::new(dir, ".pi/SYSTEM.md", MemoryKind::Instruction),
            MemorySource::new(dir, ".pi/APPEND_SYSTEM.md", MemoryKind::Instruction),
            MemorySource::new(dir, ".pi/skills/**/SKILL.md", MemoryKind::Skill),
        ]
    }
}

/// Pi's agent directory (`PI_CODING_AGENT_DIR` or `~/.pi/agent`).
fn pi_base(home: &Path) -> PathBuf {
    crate::config::resolve_agent_base("pi").unwrap_or_else(|| home.join(".pi/agent"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::MessageRole;
    use std::fs;

    const SESSION: &str = concat!(
        r#"{"type":"session","version":3,"id":"0192-aaaa","timestamp":"2026-11-21T10:43:51.669Z","cwd":"/home/me/proj"}"#,
        "\n",
        r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-11-21T10:43:51.708Z","provider":"google","modelId":"picked-model"}"#,
        "\n",
        r#"{"type":"message","id":"s1","parentId":"m1","timestamp":"2026-11-21T10:43:51.714Z","message":{"role":"system","content":"","sections":{"preamble":"You are a coding assistant"}}}"#,
        "\n",
        r#"{"type":"message","id":"u1","parentId":"s1","timestamp":"2026-11-21T10:43:52.000Z","message":{"role":"user","content":[{"type":"image","data":"AAAA"},{"type":"text","text":"count the words"},{"type":"text","text":"second part"}]}}"#,
        "\n",
        r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-11-21T10:43:53.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hidden thought"},{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"note.txt"}},{"type":"text","text":""}],"model":"model-a"}}"#,
        "\n",
        r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-11-21T10:43:54.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"read","content":[{"type":"text","text":"hello world"}]}}"#,
        "\n",
        r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-11-21T10:43:55.000Z","message":{"role":"assistant","content":[{"type":"text","text":"two words"}],"model":"model-a"}}"#,
        "\n",
        r#"{"type":"message","id":"b1","parentId":"a2","timestamp":"2026-11-21T10:43:56.000Z","message":{"role":"bashExecution","command":"wc -w note.txt","output":"2 note.txt","exitCode":0}}"#,
        "\n",
        r#"{"type":"message","id":"u2","parentId":"b1","timestamp":"2026-11-21T10:44:00.000Z","message":{"role":"user","content":"and the lines?"}}"#,
        "\n",
        r#"{"type":"message","id":"a3","parentId":"u2","timestamp":"2026-11-21T10:44:01.000Z","message":{"role":"assistant","content":[],"model":"model-failed","stopReason":"error"}}"#,
        "\n",
        r#"{"type":"message","id":"a4","parentId":"u2","timestamp":"2026-11-21T10:44:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"one line"}],"model":"model-b"}}"#,
        "\n",
    );

    fn write_session(extra: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".pi/agent/sessions/--home-me-proj--");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-11-21T10-43-51-669Z_0192-aaaa.jsonl");
        fs::write(&path, format!("{SESSION}{extra}")).unwrap();
        (tmp, path)
    }

    #[test]
    fn messages_carry_roles_and_times() {
        let (_tmp, path) = write_session("");
        let mut messages = Vec::new();
        PLUGIN.iter_messages(&path, &mut |m| {
            messages.push(m);
            true
        });
        let seen: Vec<(MessageRole, &str, Option<i64>)> = messages
            .iter()
            .map(|m| (m.role, m.text.as_str(), m.time))
            .collect();
        assert_eq!(
            seen,
            [
                // One prompt per user entry: its first text part.
                (MessageRole::User, "count the words", Some(1795257832)),
                (MessageRole::Assistant, "two words", Some(1795257835)),
                (MessageRole::User, "and the lines?", Some(1795257840)),
                (MessageRole::Assistant, "one line", Some(1795257842)),
            ]
        );
    }

    #[test]
    fn metadata_comes_from_the_header_and_file_name() {
        let (tmp, path) = write_session("");
        let home = tmp.path();
        assert_eq!(
            PLUGIN.resolve_cwd(&path, home).as_deref(),
            Some("/home/me/proj")
        );
        assert_eq!(PLUGIN.session_id(&path, home).as_deref(), Some("0192-aaaa"));
        assert_eq!(
            PLUGIN.resolve_title(&path, home).as_deref(),
            Some("count the words")
        );
        assert_eq!(
            PLUGIN.resume_args(&path, home),
            Some(vec![
                "pi".to_string(),
                "--session".to_string(),
                path.to_string_lossy().to_string()
            ])
        );
    }

    #[test]
    fn models_are_those_that_answered() {
        let (_tmp, path) = write_session("");
        // `picked-model` was only selected and `model-failed` returned nothing.
        assert_eq!(
            PLUGIN.resolve_models(&path).map(|m| m.joined()).as_deref(),
            Some("model-a,model-b")
        );
    }

    #[test]
    fn the_latest_session_name_is_the_title() {
        let name = |name: &str| {
            format!(
                "{{\"type\":\"session_info\",\"id\":\"n\",\"parentId\":\"a4\",\"timestamp\":\"2026-11-21T10:45:00.000Z\",\"name\":\"{name}\"}}\n"
            )
        };
        let (tmp, path) = write_session(&format!("{}{}", name("first"), name("Count words")));
        assert_eq!(
            PLUGIN.resolve_title(&path, tmp.path()).as_deref(),
            Some("Count words")
        );
        // An empty name clears it.
        let (tmp, path) = write_session(&format!("{}{}", name("first"), name("")));
        assert_eq!(
            PLUGIN.resolve_title(&path, tmp.path()).as_deref(),
            Some("count the words")
        );
    }

    #[test]
    fn search_texts_cover_tools_and_shell_commands() {
        let (_tmp, path) = write_session("");
        let mut texts = Vec::new();
        PLUGIN.iter_search_records(
            &path,
            &mut RecordVisitor::new(&mut |_, _, kind, text, _| {
                texts.push((kind, text.to_string()));
                true
            }),
        );
        let has = |kind, text: &str| texts.contains(&(kind, text.to_string()));
        assert!(has(K::User, "second part"));
        assert!(has(K::ToolInput, "read"));
        assert!(has(K::ToolInput, "note.txt"));
        assert!(has(K::ToolOutput, "hello world"));
        assert!(has(K::ToolInput, "wc -w note.txt"));
        assert!(has(K::ToolOutput, "2 note.txt"));
        // Neither the system prompt, thinking, nor image data is searched.
        assert!(!texts.iter().any(|(_, text)| {
            text.contains("coding assistant") || text.contains("hidden") || text == "AAAA"
        }));
        // Every line that holds a search text passes the line filter.
        for line in SESSION.lines() {
            let mut holds = false;
            PLUGIN.iter_search_texts_from_bytes(&path, line.as_bytes(), &mut |_| {
                holds = true;
                false
            });
            assert!(!holds || PLUGIN.line_may_hold_search_texts(line.as_bytes()));
        }
    }
}

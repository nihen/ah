use super::entries::{RecordVisitor, SearchKind as K, TypedVisitor};
use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use super::common::{
    SessionBytes, is_safe_cli_id, mmap_file, strip_home, visit_tool_call, visit_tool_output,
};
use super::{AgentPlugin, Message};

pub static PLUGIN: DshPlugin = DshPlugin;

/// DeepSeek Harness (`dsh`).
///
/// Layout: `~/.dsh/sessions/--<cwd>--/<session id>/session.v<N>.jsonl.zstd`
/// (`session.jsonl.zstd` for format 0, and without `.zstd` when compression
/// is turned off). The log is a header line (`id`, `cwd`, `parentSession`)
/// followed by one event per line (`type`, `seq`, `time` in milliseconds,
/// `data`). A compressed log is a concatenation of Zstandard frames: one
/// for the header line, then one per appended batch of events.
pub struct DshPlugin;

fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "zstd")
}

/// Decode the Zstandard frames of `data`, at most `max_frames` of them.
/// A frame that cannot be decoded (a write that was cut short) ends the
/// log: the frames before it are returned.
fn decode_frames(mut data: &[u8], max_frames: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..max_frames {
        if data.is_empty() {
            break;
        }
        let complete = out.len();
        let decoded = ruzstd::decoding::StreamingDecoder::new(&mut data)
            .ok()
            .and_then(|mut frame| frame.read_to_end(&mut out).ok());
        if decoded.is_none() {
            out.truncate(complete);
            break;
        }
    }
    out
}

/// The log as text: the file itself, or its decoded frames.
enum Log {
    Plain(memmap2::Mmap),
    Decoded(Rc<Vec<u8>>),
}

impl std::ops::Deref for Log {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Log::Plain(mmap) => mmap,
            Log::Decoded(bytes) => bytes,
        }
    }
}

/// The file a decoded log was read from, to tell when it has changed.
#[derive(PartialEq)]
struct Source {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

thread_local! {
    /// The log decoded last on this thread. The fields of one session are
    /// resolved one after another, each asking for the same log.
    static LAST_DECODED: RefCell<Option<(Source, Rc<Vec<u8>>)>> = const { RefCell::new(None) };
}

fn read_log(path: &Path) -> Option<Log> {
    if !is_compressed(path) {
        return mmap_file(path).map(Log::Plain);
    }
    let meta = std::fs::metadata(path).ok()?;
    let source = Source {
        path: path.to_path_buf(),
        len: meta.len(),
        modified: meta.modified().ok(),
    };
    let cached = LAST_DECODED.with_borrow(|last| {
        last.as_ref()
            .filter(|(of, _)| *of == source)
            .map(|(_, bytes)| Rc::clone(bytes))
    });
    if let Some(bytes) = cached {
        return Some(Log::Decoded(bytes));
    }
    let bytes = Rc::new(decode_frames(&mmap_file(path)?, usize::MAX));
    LAST_DECODED.with_borrow_mut(|last| *last = Some((source, Rc::clone(&bytes))));
    Some(Log::Decoded(bytes))
}

/// The header line. Only the first frame of a compressed log is decoded.
fn read_header(path: &Path) -> Option<serde_json::Value> {
    let data = mmap_file(path)?;
    let decoded;
    let text: &[u8] = if is_compressed(path) {
        decoded = decode_frames(&data, 1);
        &decoded
    } else {
        &data
    };
    header_of(text)
}

fn header_of(log: &[u8]) -> Option<serde_json::Value> {
    let line = log.split(|&b| b == b'\n').next()?;
    let val: serde_json::Value = serde_json::from_slice(line).ok()?;
    (val.get("type")?.as_str()? == "session").then_some(val)
}

/// Event types that are read. Lines of other types are skipped unparsed,
/// which matters for the token-by-token `assistant/attempt` streams.
const READ_TYPES: [&str; 4] = [
    "user/message",
    "assistant/message",
    "tool/result",
    "session/title",
];

/// Whether a line may be an event of one of `READ_TYPES`. Events are
/// written with `type` first; a line that is not is kept and parsed.
fn may_be_read(line: &[u8]) -> bool {
    match line.strip_prefix(b"{\"type\":\"") {
        Some(rest) => READ_TYPES.iter().any(|kind| {
            rest.strip_prefix(kind.as_bytes())
                .is_some_and(|r| r.starts_with(b"\""))
        }),
        None => true,
    }
}

fn for_each_event(log: &[u8], mut visit: impl FnMut(&str, &serde_json::Value) -> bool) {
    for line in log.split(|&b| b == b'\n') {
        if line.len() < 2 || !may_be_read(line) {
            continue;
        }
        let Ok(val) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let Some(kind) = val.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if !visit(kind, &val) {
            return;
        }
    }
}

/// `text` parts of a message's `content`.
fn text_parts(content: Option<&serde_json::Value>) -> impl Iterator<Item = &str> {
    content
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
}

/// The content of a `user/message` the user wrote. The harness adds its own
/// user-role messages (runtime context, the skill catalog), told apart by
/// `source.kind`.
fn prompt_content(val: &serde_json::Value) -> Option<&serde_json::Value> {
    let data = val.get("data")?;
    (data.pointer("/source/kind")?.as_str()? == "user")
        .then(|| data.get("content"))
        .flatten()
}

fn messages_of(kind: &str, val: &serde_json::Value) -> Vec<Message> {
    let time = val.get("time");
    let texts = |content| text_parts(content).filter(|text: &&str| !text.trim().is_empty());
    match kind {
        // One prompt per message: its first text part, so that the prompt
        // is one JSON string of the line.
        "user/message" => texts(prompt_content(val))
            .take(1)
            .map(|text| Message::user(text.to_string()).at(time))
            .collect(),
        "assistant/message" => texts(val.pointer("/data/message/content"))
            .map(|text| Message::assistant(text.to_string()).at(time))
            .collect(),
        _ => Vec::new(),
    }
}

/// Search texts of one event: prompts, replies with their tool calls, and
/// tool results. Returns `false` when `visit` stops.
fn visit_search_texts(kind: &str, val: &serde_json::Value, visit: &mut TypedVisitor<'_>) -> bool {
    match kind {
        "user/message" => text_parts(prompt_content(val)).all(|text| visit(K::User, text)),
        "assistant/message" => {
            let content = val.pointer("/data/message/content");
            text_parts(content).all(|text| visit(K::Assistant, text))
                && content
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter(|part| part.get("type").and_then(|v| v.as_str()) == Some("tool-call"))
                    .all(|part| {
                        visit_tool_call(part.get("name"), part.get("arguments"), &mut |text| {
                            visit(K::ToolInput, text)
                        })
                    })
        }
        "tool/result" => val.pointer("/data/message/content").is_none_or(|content| {
            visit_tool_output(content, &mut |text| visit(K::ToolOutput, text))
        }),
        _ => true,
    }
}

fn title_of(log: &[u8]) -> Option<String> {
    let mut title = None;
    let mut first_prompt = None;
    for_each_event(log, |kind, val| {
        if kind == "session/title" {
            // The latest title is current; an empty one clears it.
            title = val
                .pointer("/data/title")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_string);
        } else if first_prompt.is_none() && kind == "user/message" {
            first_prompt = messages_of(kind, val).pop().map(|m| m.text);
        }
        true
    });
    title.or(first_prompt)
}

fn header_str(header: &serde_json::Value, key: &str) -> Option<String> {
    header
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

impl AgentPlugin for DshPlugin {
    fn id(&self) -> &'static str {
        "dsh"
    }

    fn description(&self) -> &'static str {
        "DeepSeek Harness"
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

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative cwd from the session header, fallback: project dir name)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".dsh/sessions/*/*/session*.jsonl.zstd",
            ".dsh/sessions/*/*/session*.jsonl",
        ]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.dsh/sessions/"]
    }

    /// The decoded log. Decoding is not free, so it is loaded only for a
    /// full-text search.
    fn session_bytes(&self, path: &Path) -> Option<SessionBytes> {
        match read_log(path)? {
            Log::Plain(mmap) => Some(SessionBytes::Mmap(mmap)),
            Log::Decoded(bytes) => Some(SessionBytes::Owned(bytes.to_vec())),
        }
    }

    fn cheap_session_bytes(&self) -> bool {
        false
    }

    fn raw_content(&self, path: &Path) -> Option<String> {
        Some(String::from_utf8_lossy(&read_log(path)?).into_owned())
    }

    fn line_may_hold_search_texts(&self, line: &[u8]) -> bool {
        may_be_read(line)
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        if let Some(log) = read_log(path) {
            self.iter_messages_from_bytes(path, &log, visit);
        }
    }

    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        for_each_event(data, |kind, val| {
            messages_of(kind, val).into_iter().all(&mut *visit)
        });
    }

    fn iter_search_records(&self, path: &Path, visit: &mut RecordVisitor<'_>) {
        if let Some(log) = read_log(path) {
            for_each_event(&log, |kind, val| {
                visit.record(|emit| visit_search_texts(kind, val, emit))
            });
        }
    }

    fn iter_search_texts(&self, path: &Path, visit: &mut dyn FnMut(&str) -> bool) {
        if let Some(log) = read_log(path) {
            self.iter_search_texts_from_bytes(path, &log, visit);
        }
    }

    fn iter_search_texts_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(&str) -> bool,
    ) {
        for_each_event(data, |kind, val| {
            visit_search_texts(kind, val, &mut |_, text| visit(text))
        });
    }

    /// The model each reply names. A reply without content did not answer.
    fn resolve_models(&self, path: &Path) -> Option<super::common::ModelList> {
        let log = read_log(path)?;
        let mut models = super::common::ModelList::default();
        for_each_event(&log, |kind, val| {
            if kind == "assistant/message" {
                let message = val.pointer("/data/message");
                let answered = message
                    .and_then(|m| m.get("content")?.as_array())
                    .is_some_and(|parts| !parts.is_empty());
                let model = message.and_then(|m| m.pointer("/source/model")?.as_str());
                if let (true, Some(model)) = (answered, model.filter(|m| !m.is_empty())) {
                    models.push(model);
                }
            }
            true
        });
        models.non_empty()
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        self.resolve_cwd(path, home)
            .map(|cwd| strip_home(&cwd, home))
            .or_else(|| {
                let dir = path.parent()?.parent()?.file_name()?.to_str()?;
                Some(dir.trim_matches('-').to_string())
            })
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        header_str(&read_header(path)?, "cwd")
    }

    fn resolve_cwd_from_mmap(&self, _path: &Path, _home: &Path, log: &[u8]) -> Option<String> {
        header_str(&header_of(log)?, "cwd")
    }

    /// The title the harness gave the session (or the user changed it to),
    /// else the first prompt.
    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        title_of(&read_log(path)?)
    }

    fn resolve_title_from_mmap(&self, _path: &Path, _home: &Path, log: &[u8]) -> Option<String> {
        title_of(log)
    }

    /// The id in the header, or the name of the session's directory.
    fn session_id(&self, path: &Path, _home: &Path) -> Option<String> {
        read_header(path)
            .and_then(|header| header_str(&header, "id"))
            .or_else(|| Some(path.parent()?.file_name()?.to_str()?.to_string()))
            .filter(|id| is_safe_cli_id(id))
    }

    /// A subagent's header names the session that started it.
    fn parent_session_id(&self, path: &Path) -> Option<String> {
        header_str(&read_header(path)?, "parentSession")
    }

    fn is_subagent(&self, path: &Path) -> bool {
        read_header(path).is_some_and(|header| {
            header.get("origin").and_then(|v| v.as_str()) == Some("subagent")
                || header_str(&header, "parentSession").is_some()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::MessageRole;
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};
    use std::fs;

    const HEADER: &str = r#"{"type":"session","version":4,"id":"session-0192","createdAt":1795258135225,"cwd":"/home/me/proj","isSeeded":false,"delegationDepth":0}"#;
    const TURN_1: &[&str] = &[
        r#"{"type":"agent/inbox/spliced","seq":4,"time":1795258135230,"data":{"inserted":[{"content":[{"type":"text","text":"count the words"}],"source":{"kind":"user"},"role":"user"}]}}"#,
        r#"{"type":"user/message","seq":9,"time":1795258135280,"data":{"content":[{"type":"text","text":"count the words"}],"source":{"kind":"user"},"role":"user"}}"#,
        r#"{"type":"user/message","seq":10,"time":1795258135281,"data":{"content":[{"type":"text","text":"<system-reminder>runtime context</system-reminder>"}],"source":{"kind":"runtime-context"},"role":"user"}}"#,
        r#"{"type":"session/title","seq":14,"time":1795258135283,"data":{"title":"count the words","source":{"kind":"fallback"}}}"#,
        r#"{"type":"assistant/attempt","seq":15,"time":1795258135300,"data":{"stream":[{"type":"chunk","chunk":{"type":"text","text":"attempt text"}}]}}"#,
        r#"{"type":"session/title","seq":16,"time":1795258136283,"data":{"title":"Word Count","source":{"kind":"provider"}}}"#,
        r#"{"type":"assistant/message","seq":17,"time":1795258136412,"data":{"message":{"role":"assistant","content":[{"type":"tool-call","id":"c1","name":"read","arguments":"{\"file_path\":\"words.txt\"}"},{"type":"text","text":""}],"source":{"kind":"model","provider":"google","model":"model-a"}}}}"#,
        r#"{"type":"tool/call","seq":18,"time":1795258136414,"data":{"callId":"c1","name":"read","arguments":"{\"file_path\":\"words.txt\"}"}}"#,
        r#"{"type":"tool/result","seq":19,"time":1795258136420,"data":{"message":{"role":"tool","content":[{"type":"text","text":"alpha beta gamma"}]},"meta":{"path":"/home/me/proj/words.txt"}}}"#,
        r#"{"type":"assistant/message","seq":22,"time":1795258137568,"data":{"message":{"role":"assistant","content":[{"type":"text","text":"three words"}],"source":{"kind":"model","provider":"google","model":"model-a"}}}}"#,
    ];
    const TURN_2: &[&str] = &[
        r#"{"type":"user/message","seq":30,"time":1795258145300,"data":{"content":[{"type":"text","text":"and the lines?"}],"source":{"kind":"user"},"role":"user"}}"#,
        r#"{"type":"assistant/message","seq":31,"time":1795258146000,"data":{"message":{"role":"assistant","content":[],"source":{"kind":"model","model":"model-failed"}}}}"#,
        r#"{"type":"assistant/message","seq":37,"time":1795258147468,"data":{"message":{"role":"assistant","content":[{"type":"text","text":"one line"}],"source":{"kind":"model","provider":"google","model":"model-b"}}}}"#,
    ];

    fn frame(lines: &[&str]) -> Vec<u8> {
        let text = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        compress_to_vec(text.as_bytes(), CompressionLevel::Fastest)
    }

    /// A compressed log: one frame for the header, one per turn.
    fn compressed() -> Vec<u8> {
        [frame(&[HEADER]), frame(TURN_1), frame(TURN_2)].concat()
    }

    fn write_log(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join(".dsh/sessions/--home-me-proj--/session-0192");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        (tmp, path)
    }

    fn messages(path: &Path) -> Vec<(MessageRole, String, Option<i64>)> {
        let mut seen = Vec::new();
        PLUGIN.iter_messages(path, &mut |m| {
            seen.push((m.role, m.text, m.time));
            true
        });
        seen
    }

    #[test]
    fn compressed_and_plain_logs_read_the_same() {
        let (_tmp, packed) = write_log("session.v4.jsonl.zstd", &compressed());
        let plain_text = [&[HEADER][..], TURN_1, TURN_2].concat().join("\n");
        let (_tmp2, plain) = write_log("session.v4.jsonl", plain_text.as_bytes());
        let expected = vec![
            // Only what the user wrote is a prompt, once.
            (
                MessageRole::User,
                "count the words".to_string(),
                Some(1795258135),
            ),
            (
                MessageRole::Assistant,
                "three words".to_string(),
                Some(1795258137),
            ),
            (
                MessageRole::User,
                "and the lines?".to_string(),
                Some(1795258145),
            ),
            (
                MessageRole::Assistant,
                "one line".to_string(),
                Some(1795258147),
            ),
        ];
        assert_eq!(messages(&packed), expected);
        assert_eq!(messages(&plain), expected);
        assert_eq!(PLUGIN.raw_content(&packed).unwrap().trim_end(), plain_text);
    }

    #[test]
    fn metadata_comes_from_the_header_and_events() {
        let (tmp, path) = write_log("session.v4.jsonl.zstd", &compressed());
        let home = tmp.path();
        assert_eq!(
            PLUGIN.resolve_cwd(&path, home).as_deref(),
            Some("/home/me/proj")
        );
        assert_eq!(
            PLUGIN.session_id(&path, home).as_deref(),
            Some("session-0192")
        );
        assert_eq!(
            PLUGIN.resolve_title(&path, home).as_deref(),
            Some("Word Count")
        );
        // `model-failed` returned nothing.
        assert_eq!(
            PLUGIN.resolve_models(&path).map(|m| m.joined()).as_deref(),
            Some("model-a,model-b")
        );
        assert!(!PLUGIN.is_subagent(&path));
        assert_eq!(PLUGIN.resume_args(&path, home), None);
    }

    #[test]
    fn a_torn_last_frame_ends_the_log() {
        let mut bytes = compressed();
        let torn = frame(&[
            r#"{"type":"user/message","seq":40,"time":1795258150000,"data":{"content":[{"type":"text","text":"never written"}],"source":{"kind":"user"}}}"#,
        ]);
        bytes.extend_from_slice(&torn[..torn.len() - 3]);
        let (tmp, path) = write_log("session.v4.jsonl.zstd", &bytes);
        assert_eq!(messages(&path).len(), 4);
        assert_eq!(
            PLUGIN.resolve_title(&path, tmp.path()).as_deref(),
            Some("Word Count")
        );
        // A file that is not Zstandard at all reads as empty.
        let (tmp, path) = write_log("session.v4.jsonl.zstd", b"not a zstd file");
        assert!(messages(&path).is_empty());
        assert_eq!(
            PLUGIN.session_id(&path, tmp.path()).as_deref(),
            Some("session-0192")
        );
    }

    #[test]
    fn a_changed_file_is_decoded_again() {
        let (_tmp, path) = write_log(
            "session.v4.jsonl.zstd",
            &[frame(&[HEADER]), frame(TURN_1)].concat(),
        );
        assert_eq!(messages(&path).len(), 2);
        fs::write(&path, compressed()).unwrap();
        assert_eq!(messages(&path).len(), 4);
    }

    #[test]
    fn subagents_name_their_parent() {
        let header = r#"{"type":"session","version":4,"id":"0192-child","createdAt":1795258444078,"cwd":"/home/me/proj","parentSession":"session-0192","isSeeded":false,"origin":"subagent","delegationDepth":1}"#;
        let (tmp, path) = write_log("session.v4.jsonl.zstd", &frame(&[header]));
        assert!(PLUGIN.is_subagent(&path));
        assert_eq!(
            PLUGIN.parent_session_id(&path).as_deref(),
            Some("session-0192")
        );
        assert_eq!(
            PLUGIN.session_id(&path, tmp.path()).as_deref(),
            Some("0192-child")
        );
    }

    #[test]
    fn search_texts_cover_tool_calls_and_results() {
        let (_tmp, path) = write_log("session.v4.jsonl.zstd", &compressed());
        let mut texts = Vec::new();
        PLUGIN.iter_search_records(
            &path,
            &mut RecordVisitor::new(&mut |_, _, kind, text, _| {
                texts.push((kind, text.to_string()));
                true
            }),
        );
        let has = |kind, text: &str| texts.contains(&(kind, text.to_string()));
        assert!(has(K::User, "count the words"));
        assert!(has(K::ToolInput, "read"));
        assert!(has(K::ToolInput, r#"{"file_path":"words.txt"}"#));
        assert!(has(K::ToolOutput, "alpha beta gamma"));
        assert!(has(K::Assistant, "three words"));
        // Each prompt and tool call is searched once, and neither the
        // harness's own messages nor attempt streams are.
        assert_eq!(
            texts.iter().filter(|(_, t)| t == "count the words").count(),
            1
        );
        assert_eq!(texts.iter().filter(|(_, t)| t == "read").count(), 1);
        assert!(
            !texts
                .iter()
                .any(|(_, t)| t.contains("runtime context") || t.contains("attempt"))
        );
    }
}

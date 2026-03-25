use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use regex::Regex;
use regex::bytes::Regex as BytesRegex;

use crate::agents::common::{format_mtime, mmap_file};
use crate::agents::{AgentPlugin, Message, MessageRole};
use crate::cli::{Field, SearchMode};

/// Fields that require iterating through all messages.
const MESSAGE_FIELDS: &[Field] = &[
    Field::FirstPrompt,
    Field::LastPrompt,
    Field::Prompts,
    Field::Responses,
    Field::Messages,
    Field::Transcript,
    Field::Turns,
];

/// Fields that require collecting ALL messages (not just the first user message).
const HEAVY_MESSAGE_FIELDS: &[Field] = &[
    Field::LastPrompt,
    Field::Prompts,
    Field::Responses,
    Field::Messages,
    Field::Transcript,
];

fn default_project(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn project_name(value: &str) -> String {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        return "?".to_string();
    }

    PathBuf::from(trimmed)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| trimmed.to_string())
}

fn resolve_project_fallback(path: &Path, plugin: &dyn AgentPlugin, home: &Path) -> String {
    plugin
        .resolve_project(path, home)
        .map(|value| project_name(&value))
        .unwrap_or_else(|| default_project(path))
}

fn resolve_date(path: &Path, plugin: &dyn AgentPlugin, mtime: SystemTime) -> String {
    plugin
        .resolve_date(path, mtime)
        .unwrap_or_else(|| format_mtime(mtime))
}

fn resolve_created(path: &Path, mtime: SystemTime) -> String {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.created().ok())
        .map(format_mtime)
        .unwrap_or_else(|| format_mtime(mtime))
}

fn truncate_chars(s: &str, limit: usize) -> String {
    if limit == 0 || s.chars().count() <= limit {
        return s.to_string();
    }
    let truncated: String = s.chars().take(limit).collect();
    format!("{}..", truncated)
}

fn resolve_size(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{}'", escaped)
}

fn resolve_resume_command(
    path: &Path,
    plugin: &dyn AgentPlugin,
    home: &Path,
    cwd: &Option<String>,
) -> String {
    let Some(args) = plugin.resume_args(path, home) else {
        return String::new();
    };
    if args.is_empty() {
        return String::new();
    }

    let command = args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");

    match cwd {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), command),
        None => command,
    }
}

/// Resolve Matched field. In All mode, searches raw file via mmap.
/// In Prompt mode, falls back to iterating user messages only.
fn resolve_matched(
    path: &Path,
    plugin: &dyn AgentPlugin,
    opts: &ResolveOpts,
    preloaded_mmap: Option<&[u8]>,
) -> String {
    match opts.search_mode {
        SearchMode::All => {
            let Some(bytes_re) = &opts.bytes_query_re else {
                return String::new();
            };
            resolve_matched_mmap(
                &plugin.search_path(path),
                bytes_re,
                preloaded_mmap,
                opts.literal_needle.as_deref(),
            )
        }
        SearchMode::Prompt => {
            let Some(re) = &opts.query_re else {
                return String::new();
            };
            resolve_matched_prompts(path, plugin, re)
        }
    }
}

/// Fast path: search raw file bytes via mmap.
/// Uses bytes::Regex to find match position, then converts only the
/// surrounding ~200 bytes to UTF-8 for context extraction.
fn resolve_matched_mmap(
    path: &Path,
    bytes_re: &BytesRegex,
    preloaded: Option<&[u8]>,
    literal_needle: Option<&[u8]>,
) -> String {
    let mmap_holder;
    let data: &[u8] = if let Some(d) = preloaded {
        d
    } else {
        mmap_holder = mmap_file(path);
        match &mmap_holder {
            Some(m) => m,
            None => return String::new(),
        }
    };
    // Fast literal path: use memchr SIMD instead of regex
    if let Some(needle) = literal_needle {
        return extract_matches_literal_bytes(data, needle);
    }
    // Regex path: find match in raw bytes, then extract context from surrounding slice
    if let Some(mat) = bytes_re.find(data) {
        extract_context_from_bytes(data, mat.start(), mat.end(), 30)
    } else {
        String::new()
    }
}

/// Extract match context using fast case-insensitive byte search.
/// Works directly on raw bytes — no full-file UTF-8 conversion needed.
fn extract_matches_literal_bytes(data: &[u8], needle_lower: &[u8]) -> String {
    if needle_lower.is_empty() {
        return String::new();
    }
    let first_lower = needle_lower[0];
    let first_upper = first_lower.to_ascii_uppercase();
    let has_case = first_lower != first_upper;

    let mut start = 0;
    loop {
        if start + needle_lower.len() > data.len() {
            return String::new();
        }
        let found = if has_case {
            memchr::memchr2(first_lower, first_upper, &data[start..])
        } else {
            memchr::memchr(first_lower, &data[start..])
        };
        let Some(pos) = found else {
            return String::new();
        };
        let abs = start + pos;
        if abs + needle_lower.len() > data.len() {
            return String::new();
        }
        if data[abs..abs + needle_lower.len()]
            .iter()
            .zip(needle_lower)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
        {
            return extract_context_from_bytes(data, abs, abs + needle_lower.len(), 30);
        }
        start = abs + 1;
    }
}

/// Extract context snippet from raw bytes around a match position.
fn extract_context_from_bytes(data: &[u8], start: usize, end: usize, max_context: usize) -> String {
    // Take a generous byte window around the match (4 bytes per char max in UTF-8)
    let window = max_context.saturating_mul(4);
    let mut slice_start = start.saturating_sub(window);
    let slice_end = end.saturating_add(window).min(data.len());

    // Align slice_start to a UTF-8 char boundary (skip continuation bytes 10xxxxxx)
    while slice_start < start && data[slice_start] & 0xC0 == 0x80 {
        slice_start += 1;
    }

    let slice = &data[slice_start..slice_end];
    let text = match std::str::from_utf8(slice) {
        Ok(t) => t,
        Err(e) => {
            let valid_end = e.valid_up_to();
            if valid_end == 0 {
                return String::new();
            }
            match std::str::from_utf8(&slice[..valid_end]) {
                Ok(t) => t,
                Err(_) => return String::new(),
            }
        }
    };

    // Adjust match positions relative to the slice, snapping to char boundaries
    let mut rel_start = start - slice_start;
    let mut rel_end = end - slice_start;
    if rel_end > text.len() {
        return String::new();
    }
    // Snap to nearest UTF-8 char boundaries (bytes::Regex may return mid-char offsets)
    while rel_start > 0 && !text.is_char_boundary(rel_start) {
        rel_start -= 1;
    }
    while rel_end < text.len() && !text.is_char_boundary(rel_end) {
        rel_end += 1;
    }

    extract_match_context(text, rel_start, rel_end, max_context)
}

/// Slow path: search only user prompt messages.
fn resolve_matched_prompts(path: &Path, plugin: &dyn AgentPlugin, re: &Regex) -> String {
    let mut result = String::new();
    plugin.iter_messages(path, &mut |message| {
        if message.role != MessageRole::User {
            return true;
        }
        let text = &message.text;
        if let Some(mat) = re.find(text) {
            result = extract_match_context(text, mat.start(), mat.end(), 30);
            return false;
        }
        true
    });
    result
}

/// Extract a snippet around a match, keeping total ~max_context chars.
/// Tries to split context evenly before/after the match.
/// Only examines bytes near the match position — O(max_context), not O(file_size).
fn extract_match_context(text: &str, start: usize, end: usize, max_context: usize) -> String {
    // Validate that start/end are within bounds and on UTF-8 char boundaries
    if start > end
        || end > text.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        return String::new();
    }

    // Count chars in the match span
    let match_text = &text[start..end];
    let match_len_chars = match_text.chars().count();
    let remaining = max_context.saturating_sub(match_len_chars);
    let before_chars = remaining / 2;
    let after_chars = remaining - before_chars;

    // Walk backward from start to find ctx_start_byte (before_chars chars back)
    let ctx_start_byte = if before_chars == 0 {
        start
    } else {
        text[..start]
            .char_indices()
            .rev()
            .nth(before_chars - 1)
            .map(|(i, _)| i)
            .unwrap_or(0)
    };

    // Walk forward from end to find ctx_end_byte (after_chars chars forward)
    let ctx_end_byte = text[end..]
        .char_indices()
        .nth(after_chars)
        .map(|(i, _)| end + i)
        .unwrap_or(text.len());

    let mut result = String::new();
    if ctx_start_byte > 0 {
        result.push_str("...");
    }
    result.push_str(&text[ctx_start_byte..ctx_end_byte]);
    if ctx_end_byte < text.len() {
        result.push_str("...");
    }
    result
}

/// Collect all messages once and derive multiple fields from them.
fn resolve_message_fields(
    messages: &[Message],
    fields: &[Field],
    transcript_limit: usize,
) -> BTreeMap<Field, String> {
    let mut map = BTreeMap::new();

    for field in fields {
        if !MESSAGE_FIELDS.contains(field) {
            continue;
        }
        let value = match field {
            Field::FirstPrompt => messages
                .iter()
                .find(|m| m.role == MessageRole::User)
                .map(|m| m.text.clone())
                .unwrap_or_default(),
            Field::LastPrompt => messages
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::User)
                .map(|m| m.text.clone())
                .unwrap_or_default(),
            Field::Prompts => {
                let arr: Vec<_> = messages
                    .iter()
                    .filter(|m| m.role == MessageRole::User)
                    .map(|m| serde_json::Value::String(m.text.clone()))
                    .collect();
                serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
            }
            Field::Responses => {
                let arr: Vec<_> = messages
                    .iter()
                    .filter(|m| m.role == MessageRole::Assistant)
                    .map(|m| serde_json::Value::String(m.text.clone()))
                    .collect();
                serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
            }
            Field::Messages => {
                let arr: Vec<_> = messages
                    .iter()
                    .map(|m| {
                        let role = match m.role {
                            MessageRole::User => "user",
                            MessageRole::Assistant => "assistant",
                        };
                        let mut item = serde_json::Map::new();
                        item.insert(
                            "role".to_string(),
                            serde_json::Value::String(role.to_string()),
                        );
                        item.insert(
                            "text".to_string(),
                            serde_json::Value::String(m.text.clone()),
                        );
                        serde_json::Value::Object(item)
                    })
                    .collect();
                serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
            }
            Field::Transcript => {
                let mut body = String::new();
                for m in messages {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&m.text);
                    if transcript_limit > 0 && body.chars().count() >= transcript_limit {
                        body = body.chars().take(transcript_limit).collect();
                        body.push_str("..");
                        break;
                    }
                }
                body
            }
            Field::Turns => messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .count()
                .to_string(),
            _ => unreachable!(),
        };
        map.insert(*field, value);
    }

    map
}

/// Options for resolve_fields that remain constant across files.
pub struct ResolveOpts {
    pub transcript_limit: usize,
    pub title_limit: usize,
    pub query_re: Option<Regex>,
    pub bytes_query_re: Option<BytesRegex>,
    pub search_mode: SearchMode,
    /// Pre-computed lowercase needle for fast literal match context extraction.
    pub literal_needle: Option<Vec<u8>>,
}

impl ResolveOpts {
    pub fn new(query: &str, transcript_limit: usize, title_limit: usize) -> Self {
        let query_re = if !query.is_empty() {
            Regex::new(&format!("(?i){}", query)).ok()
        } else {
            None
        };
        let bytes_query_re = if !query.is_empty() {
            BytesRegex::new(&format!("(?iu){}", query)).ok()
        } else {
            None
        };
        let literal_needle = if !query.is_empty() && Self::is_literal_ascii(query) {
            Some(
                query
                    .as_bytes()
                    .iter()
                    .map(|b| b.to_ascii_lowercase())
                    .collect(),
            )
        } else {
            None
        };
        Self {
            transcript_limit,
            title_limit,
            query_re,
            bytes_query_re,
            search_mode: SearchMode::All,
            literal_needle,
        }
    }

    fn is_literal_ascii(query: &str) -> bool {
        query.is_ascii()
            && !query.chars().any(|c| {
                matches!(
                    c,
                    '\\' | '.'
                        | '^'
                        | '$'
                        | '*'
                        | '+'
                        | '?'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '|'
                )
            })
    }

    pub fn with_search_mode(mut self, mode: SearchMode) -> Self {
        self.search_mode = mode;
        self
    }

    pub fn default_with_title_limit(title_limit: usize) -> Self {
        Self {
            title_limit,
            ..Default::default()
        }
    }
}

impl Default for ResolveOpts {
    fn default() -> Self {
        Self {
            transcript_limit: 0,
            title_limit: 0,
            query_re: None,
            bytes_query_re: None,
            search_mode: SearchMode::All,
            literal_needle: None,
        }
    }
}

pub fn resolve_fields(
    path: &Path,
    plugin: &dyn AgentPlugin,
    mtime: SystemTime,
    home: &Path,
    fields: &[Field],
    opts: &ResolveOpts,
) -> BTreeMap<Field, String> {
    resolve_fields_with_mmap(path, plugin, mtime, home, fields, opts, None)
}

pub fn resolve_fields_with_mmap(
    path: &Path,
    plugin: &dyn AgentPlugin,
    mtime: SystemTime,
    home: &Path,
    fields: &[Field],
    opts: &ResolveOpts,
    preloaded_mmap: Option<&[u8]>,
) -> BTreeMap<Field, String> {
    let mut map = BTreeMap::new();

    // Resolve cwd once (needed by Project, Cwd, ResumeCmd)
    let needs_cwd = fields.iter().any(|f| {
        matches!(
            f,
            Field::Cwd | Field::Project | Field::ProjectRaw | Field::ResumeCmd
        )
    });
    let cwd = if needs_cwd {
        if let Some(mmap) = preloaded_mmap {
            plugin.resolve_cwd_from_mmap(path, home, mmap)
        } else {
            plugin.resolve_cwd(path, home)
        }
    } else {
        None
    };

    // Try resolving title via plugin first (fast path: no message iteration needed)
    let plugin_title = if fields.contains(&Field::Title) {
        if let Some(mmap) = preloaded_mmap {
            plugin.resolve_title_from_mmap(path, home, mmap)
        } else {
            plugin.resolve_title(path, home)
        }
    } else {
        None
    };

    // Determine what level of message iteration is needed.
    // Title falls back to FirstPrompt, so include it when Title is requested
    // AND plugin didn't provide a title.
    let needs_first_prompt_for_title = fields.contains(&Field::Title)
        && plugin_title.is_none()
        && !fields.contains(&Field::FirstPrompt);
    let needs_any_message =
        fields.iter().any(|f| MESSAGE_FIELDS.contains(f)) || needs_first_prompt_for_title;
    let needs_heavy = fields.iter().any(|f| HEAVY_MESSAGE_FIELDS.contains(f));

    let resolve_message_fields_list: Vec<Field> = if needs_first_prompt_for_title {
        let mut list: Vec<Field> = fields.to_vec();
        list.push(Field::FirstPrompt);
        list
    } else {
        fields.to_vec()
    };

    // Use iter_messages_from_bytes when we have pre-loaded mmap to avoid redundant file I/O
    macro_rules! iter_msgs {
        ($plugin:expr, $path:expr, $visit:expr) => {
            if let Some(data) = preloaded_mmap {
                $plugin.iter_messages_from_bytes($path, data, $visit);
            } else {
                $plugin.iter_messages($path, $visit);
            }
        };
    }

    let message_results = if needs_heavy {
        let mut collected = Vec::new();
        iter_msgs!(plugin, path, &mut |message| {
            collected.push(message);
            true
        });
        resolve_message_fields(
            &collected,
            &resolve_message_fields_list,
            opts.transcript_limit,
        )
    } else if needs_any_message {
        let needs_first_prompt = resolve_message_fields_list.contains(&Field::FirstPrompt);
        let needs_turns = resolve_message_fields_list.contains(&Field::Turns);
        let mut first_prompt: Option<String> = None;
        let mut turn_count: usize = 0;

        iter_msgs!(plugin, path, &mut |message| {
            if message.role == MessageRole::User {
                turn_count += 1;
                if first_prompt.is_none() && needs_first_prompt {
                    first_prompt = Some(message.text.clone());
                    if !needs_turns {
                        return false;
                    }
                }
            }
            true
        });

        let mut result = BTreeMap::new();
        if let Some(fp) = first_prompt {
            result.insert(Field::FirstPrompt, fp);
        }
        if needs_turns {
            result.insert(Field::Turns, turn_count.to_string());
        }
        result
    } else {
        BTreeMap::new()
    };

    // Title may fall back to first_prompt (first line, truncated) from message_results
    let title_fallback = || {
        message_results
            .get(&Field::FirstPrompt)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let first_line = s.lines().next().unwrap_or(s);
                truncate_chars(first_line, opts.title_limit)
            })
    };

    for field in fields {
        if let Some(value) = message_results.get(field) {
            map.insert(*field, value.clone());
            continue;
        }

        let value = match field {
            Field::Agent => crate::config::find_agent_for_path(path)
                .map(|a| a.id.clone())
                .unwrap_or_else(|| plugin.id().to_string()),
            Field::Project => match cwd {
                Some(ref cwd_str) => project_name(cwd_str),
                None => resolve_project_fallback(path, plugin, home),
            },
            Field::ProjectRaw => plugin
                .resolve_project(path, home)
                .unwrap_or_else(|| default_project(path)),
            Field::ModifiedAt => resolve_date(path, plugin, mtime),
            Field::CreatedAt => resolve_created(path, mtime),
            Field::Title => plugin_title
                .clone()
                .map(|s| truncate_chars(&s, opts.title_limit))
                .or_else(title_fallback)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    path.file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_default(),
            Field::Path => path.to_string_lossy().to_string(),
            Field::Cwd => cwd.clone().unwrap_or_default(),
            Field::Id => plugin.resolve_resume_id(path, home).unwrap_or_default(),
            Field::ResumeCmd => resolve_resume_command(path, plugin, home, &cwd),
            Field::Size => resolve_size(path).to_string(),
            Field::Matched => resolve_matched(path, plugin, opts, preloaded_mmap),
            _ => String::new(),
        };
        map.insert(*field, value);
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::find_plugin;
    use std::path::PathBuf;
    use std::sync::Once;

    static INIT: Once = Once::new();

    fn ensure_config() {
        INIT.call_once(|| {
            let home = crate::agents::common::canonical_home();
            crate::config::init(&home);
        });
    }

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn opts(transcript_limit: usize, title_limit: usize) -> ResolveOpts {
        ResolveOpts::new("", transcript_limit, title_limit)
    }

    #[test]
    fn test_resolve_fields_claude() {
        ensure_config();
        let path = fixture_path("claude_session.jsonl");
        let plugin = find_plugin("claude").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![
            Field::Agent,
            Field::Project,
            Field::Title,
            Field::Turns,
            Field::Size,
            Field::FirstPrompt,
        ];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 30),
        );
        assert_eq!(result.get(&Field::Agent).unwrap(), "claude");
        assert_eq!(result.get(&Field::Project).unwrap(), "myproject");
        assert_eq!(result.get(&Field::Title).unwrap(), "fix-auth-bug");
        assert_eq!(result.get(&Field::Turns).unwrap(), "2");
        assert_eq!(result.get(&Field::FirstPrompt).unwrap(), "fix the auth bug");
        assert!(result.get(&Field::Size).unwrap().parse::<u64>().unwrap() > 0);
    }

    #[test]
    fn test_resolve_project_codex_basename() {
        let path = fixture_path("codex_session.jsonl");
        let plugin = find_plugin("codex").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![Field::Project];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 30),
        );
        assert_eq!(result.get(&Field::Project).unwrap(), "api-server");
    }

    #[test]
    fn test_resolve_transcript_gemini() {
        let path = fixture_path("gemini_session.json");
        let plugin = find_plugin("gemini").unwrap();
        let fields = vec![Field::Transcript];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            Path::new("/"),
            &fields,
            &opts(500, 30),
        );
        let body = result.get(&Field::Transcript).unwrap();
        assert!(body.contains("review this plan"));
        assert!(body.contains("Looks good."));
    }

    #[test]
    fn test_resolve_messages_cursor() {
        let path = fixture_path("cursor_session.jsonl");
        let plugin = find_plugin("cursor").unwrap();
        let fields = vec![Field::Messages];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            Path::new("/"),
            &fields,
            &opts(500, 30),
        );
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(result.get(&Field::Messages).unwrap()).unwrap();
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
    }

    #[test]
    fn test_resolve_turns_gemini() {
        let path = fixture_path("gemini_session.json");
        let plugin = find_plugin("gemini").unwrap();
        let fields = vec![Field::Turns];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            Path::new("/"),
            &fields,
            &opts(500, 30),
        );
        assert_eq!(result.get(&Field::Turns).unwrap(), "2");
    }

    #[test]
    fn test_resolve_matched_prompt_only() {
        let path = fixture_path("claude_session.jsonl");
        let plugin = find_plugin("claude").unwrap();
        let fields = vec![Field::Matched];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            Path::new("/"),
            &fields,
            &ResolveOpts::new("auth", 500, 30),
        );
        assert!(result.get(&Field::Matched).unwrap().contains("auth"));
    }

    #[test]
    fn test_resolve_resume_command_claude() {
        let path = fixture_path("claude_session.jsonl");
        let plugin = find_plugin("claude").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![Field::ResumeCmd];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 30),
        );
        assert_eq!(
            result.get(&Field::ResumeCmd),
            Some(&"cd '/Users/test/myproject' && 'claude' '--resume' 'claude_session'".to_string())
        );
    }

    #[test]
    fn test_resolve_resume_command_codex() {
        let path = fixture_path("codex_session.jsonl");
        let plugin = find_plugin("codex").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![Field::ResumeCmd];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 30),
        );
        assert_eq!(
            result.get(&Field::ResumeCmd),
            Some(&"cd '/Users/test/api-server' && 'codex' 'resume' 'codex-sess-001'".to_string())
        );
    }

    #[test]
    fn test_truncate_chars() {
        assert_eq!(truncate_chars("short", 30), "short");
        assert_eq!(truncate_chars("hello world", 5), "hello..");
        assert_eq!(truncate_chars("exact", 5), "exact");
        assert_eq!(truncate_chars("日本語テスト", 3), "日本語..");
        assert_eq!(truncate_chars("anything", 0), "anything");
    }

    #[test]
    fn test_title_fallback_truncated() {
        let path = fixture_path("codex_session.jsonl");
        let plugin = find_plugin("codex").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![Field::Title];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 10),
        );
        let title = result.get(&Field::Title).unwrap();
        assert!(title.ends_with("..") || title.chars().count() <= 10);
    }

    #[test]
    fn test_title_fallback_without_first_prompt_field() {
        let path = fixture_path("codex_session.jsonl");
        let plugin = find_plugin("codex").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![Field::Title];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 30),
        );
        let title = result.get(&Field::Title).unwrap();
        assert!(
            !title.is_empty(),
            "Title should not be empty when first prompt exists"
        );
    }

    #[test]
    fn test_title_explicit_truncated_by_limit() {
        let path = fixture_path("claude_session.jsonl");
        let plugin = find_plugin("claude").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![Field::Title];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 5),
        );
        assert_eq!(result.get(&Field::Title).unwrap(), "fix-a..");
    }

    #[test]
    fn test_title_explicit_no_truncate_when_short() {
        let path = fixture_path("claude_session.jsonl");
        let plugin = find_plugin("claude").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![Field::Title];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 50),
        );
        assert_eq!(result.get(&Field::Title).unwrap(), "fix-auth-bug");
    }

    #[test]
    fn test_title_no_limit() {
        let path = fixture_path("codex_session.jsonl");
        let plugin = find_plugin("codex").unwrap();
        let home = Path::new("/Users/test");
        let fields = vec![Field::Title];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            home,
            &fields,
            &opts(500, 0),
        );
        let title = result.get(&Field::Title).unwrap();
        assert!(!title.is_empty());
        assert!(!title.ends_with(".."), "title_limit=0 should not truncate");
    }

    #[test]
    fn test_cursor_title_unwraps_user_query() {
        let path = fixture_path("cursor_user_query.jsonl");
        let plugin = find_plugin("cursor").unwrap();
        let fields = vec![Field::Title];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            Path::new("/"),
            &fields,
            &opts(500, 80),
        );
        assert_eq!(result.get(&Field::Title).unwrap(), "wrapped prompt line");
    }

    #[test]
    fn test_claude_user_content_array() {
        let path = fixture_path("claude_user_array.jsonl");
        let plugin = find_plugin("claude").unwrap();
        let fields = vec![Field::FirstPrompt];
        let result = resolve_fields(
            &path,
            plugin,
            SystemTime::now(),
            Path::new("/Users/test"),
            &fields,
            &opts(500, 30),
        );
        assert_eq!(
            result.get(&Field::FirstPrompt).unwrap(),
            "from array content"
        );
    }

    #[test]
    fn test_title_fallback_to_stem() {
        let plugin = find_plugin("cursor").unwrap();
        let dir = std::env::temp_dir().join(format!("ah-title-stem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("only_assistant.jsonl");
        std::fs::write(
            &empty,
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
        )
        .unwrap();
        let fields = vec![Field::Title];
        let result = resolve_fields(
            &empty,
            plugin,
            SystemTime::now(),
            Path::new("/"),
            &fields,
            &opts(500, 30),
        );
        assert_eq!(result.get(&Field::Title).unwrap(), "only_assistant");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_matched_mmap_basic() {
        let path = fixture_path("claude_session.jsonl");
        let re = BytesRegex::new("(?iu)auth").unwrap();
        let result = resolve_matched_mmap(&path, &re, None, None);
        assert!(!result.is_empty());
        assert!(result.contains("auth"));
    }

    #[test]
    fn test_resolve_matched_mmap_no_match() {
        let path = fixture_path("claude_session.jsonl");
        let re = BytesRegex::new("ZZZZNOTEXIST").unwrap();
        let result = resolve_matched_mmap(&path, &re, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_resolve_matched_mmap_nonexistent_file() {
        let re = BytesRegex::new("test").unwrap();
        let result = resolve_matched_mmap(Path::new("/nonexistent"), &re, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_resolve_matched_mmap_returns_first_match_only() {
        let dir = std::env::temp_dir().join(format!("ah-matched-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("many_matches.txt");
        let content = (0..10)
            .map(|i| format!("line {} hello world", i))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, content).unwrap();
        let re = BytesRegex::new("hello").unwrap();
        let result = resolve_matched_mmap(&path, &re, None, None);
        // Returns only the first match context (no " | " separator)
        assert!(result.contains("hello"));
        assert!(
            !result.contains(" | "),
            "Should return single excerpt, got: {}",
            result
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_matched_prompts() {
        let path = fixture_path("claude_session.jsonl");
        let plugin = find_plugin("claude").unwrap();
        let re = Regex::new("(?i)auth").unwrap();
        let result = resolve_matched_prompts(&path, plugin, &re);
        assert!(result.contains("auth"));
    }

    #[test]
    fn test_resolve_matched_prompts_no_assistant_match() {
        // "fix" appears only in assistant response "I'll fix that for you."
        let path = fixture_path("claude_session.jsonl");
        let plugin = find_plugin("claude").unwrap();
        let re = Regex::new("(?i)fix that").unwrap();
        let result = resolve_matched_prompts(&path, plugin, &re);
        // Should NOT match since it's in assistant text, not user prompt
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_match_context_basic() {
        let text = "hello world, this is a test string";
        let start = text.find("test").unwrap();
        let end = start + "test".len();
        let result = extract_match_context(text, start, end, 10);
        assert!(result.contains("test"));
    }

    #[test]
    fn test_extract_match_context_no_before_context() {
        // When match is long enough that before_chars == 0
        let text = "abcdefghijklmnopqrstuvwxyz";
        // Match the entire string (match_len_chars >= max_context)
        let result = extract_match_context(text, 0, text.len(), 5);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_extract_match_context_multibyte() {
        let text = "こんにちは世界テスト文字列";
        // Find "テスト" in the string
        let start = text.find("テスト").unwrap();
        let end = start + "テスト".len();
        let result = extract_match_context(text, start, end, 10);
        assert!(result.contains("テスト"));
    }

    #[test]
    fn test_extract_match_context_invalid_boundary() {
        let text = "café";
        // 'é' is 2 bytes (0xC3 0xA9); byte offset 3 is the start of 'é' and byte offset 4 is mid-character
        let result = extract_match_context(text, 4, 5, 10);
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_match_context_at_start() {
        let text = "hello world";
        let result = extract_match_context(text, 0, 5, 10);
        assert!(result.starts_with("hello"));
    }

    #[test]
    fn test_extract_match_context_at_end() {
        let text = "hello world";
        let result = extract_match_context(text, 6, 11, 10);
        assert!(result.ends_with("world"));
    }

    #[test]
    fn test_extract_context_from_bytes_ascii() {
        let data = b"hello world test string";
        let start = 12; // "test"
        let end = 16;
        let result = extract_context_from_bytes(data, start, end, 10);
        assert!(result.contains("test"));
    }

    #[test]
    fn test_extract_context_from_bytes_multibyte() {
        let text = "前後のテスト文字列です";
        let data = text.as_bytes();
        let start = text.find("テスト").unwrap();
        let end = start + "テスト".len();
        let result = extract_context_from_bytes(data, start, end, 10);
        assert!(result.contains("テスト"));
    }

    #[test]
    fn test_extract_context_from_bytes_boundary_snap() {
        // Simulate bytes::Regex returning mid-char offset
        let text = "cafébar";
        let data = text.as_bytes();
        // 'é' = bytes [4,5], so offset 5 is mid-char in the 'é'
        // This should snap and still produce output
        let result = extract_context_from_bytes(data, 4, 6, 10);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_claude_first_user_prompt_from_mmap() {
        use crate::agents::claude::ClaudePlugin;
        let path = fixture_path("claude_session.jsonl");
        let mmap = mmap_file(&path).unwrap();
        let result = ClaudePlugin::first_user_prompt_from_mmap(&mmap);
        assert_eq!(result, Some("fix the auth bug".to_string()));
    }

    #[test]
    fn test_claude_first_user_prompt_from_mmap_skips_system() {
        use crate::agents::claude::ClaudePlugin;
        let path = fixture_path("claude_session.jsonl");
        let mmap = mmap_file(&path).unwrap();
        let result = ClaudePlugin::first_user_prompt_from_mmap(&mmap);
        // Should skip the <command-name> line and return first real user prompt
        assert_eq!(result, Some("fix the auth bug".to_string()));
    }

    #[test]
    fn test_claude_first_user_prompt_from_mmap_content_array() {
        use crate::agents::claude::ClaudePlugin;
        let path = fixture_path("claude_user_array.jsonl");
        let mmap = mmap_file(&path).unwrap();
        let result = ClaudePlugin::first_user_prompt_from_mmap(&mmap);
        assert_eq!(result, Some("from array content".to_string()));
    }

    #[test]
    fn test_resolve_opts_default() {
        let opts = ResolveOpts::default();
        assert_eq!(opts.transcript_limit, 0);
        assert_eq!(opts.title_limit, 0);
        assert!(opts.query_re.is_none());
    }

    #[test]
    fn test_resolve_opts_default_with_title_limit() {
        let opts = ResolveOpts::default_with_title_limit(50);
        assert_eq!(opts.title_limit, 50);
        assert_eq!(opts.transcript_limit, 0);
        assert!(opts.query_re.is_none());
    }

    #[test]
    fn test_resolve_opts_new_with_query() {
        let opts = ResolveOpts::new("auth", 500, 30);
        assert_eq!(opts.transcript_limit, 500);
        assert_eq!(opts.title_limit, 30);
        assert!(opts.query_re.is_some());
    }

    #[test]
    fn test_resolve_opts_new_empty_query() {
        let opts = ResolveOpts::new("", 0, 0);
        assert!(opts.query_re.is_none());
    }
}

use std::borrow::Cow;
use std::path::Path;

use regex::Regex;
use regex::bytes::Regex as BytesRegex;

use crate::agents::AgentPlugin;
use crate::agents::{Message, MessageRole};

/// Full-text search: check if file matches (early exit).
/// NOTE: Pipeline now does mmap+search directly for better mmap sharing.
/// This function is kept for tests.
#[cfg(test)]
pub fn search_fulltext_matches(
    path: &Path,
    plugin: &dyn AgentPlugin,
    pattern: &BytesRegex,
) -> bool {
    match plugin.session_bytes(path) {
        Some(bytes) => pattern.is_match(&bytes),
        None => false,
    }
}

/// Prompt-only search through the plugin-provided user messages.
/// `data` is the session file's content when already loaded.
pub fn search_prompts_matches(
    path: &Path,
    plugin: &dyn AgentPlugin,
    pattern: &Regex,
    data: Option<&[u8]>,
) -> bool {
    let mut found = false;
    let mut visit = |message: Message| {
        if message.role == MessageRole::User && pattern.is_match(&message.text) {
            found = true;
            false
        } else {
            true
        }
    };
    match data {
        Some(data) => plugin.iter_messages_from_bytes(path, data, &mut visit),
        None => plugin.iter_messages(path, &mut visit),
    }
    found
}

/// Raw-bytes check that lets prompt-only search skip JSON parsing.
///
/// Only built for literal queries (no regex metacharacters). A prompt is a
/// substring of a JSON string, so every character of a match is either
/// stored verbatim — then the raw bytes contain the query — or written as an
/// escape (`\"`, `\n`, `\u00e9`, ...). `may_match` therefore returns `false`
/// only when the raw bytes lack the query and no escape decodes to a
/// character that the query can match case-insensitively.
pub struct PromptPrefilter {
    needle: BytesRegex,
    /// Sorted characters that equal a query character under the same
    /// case-insensitive rules as the prompt regex.
    matching_chars: Vec<char>,
    /// Indexed by the byte after `\`: whether that short escape can decode to
    /// a query character. `u` is handled separately.
    short_escape: [bool; 256],
}

/// Positions of the next needle match and the next relevant escape at or
/// after the scan offset (`usize::MAX` when there is none), each recomputed
/// only once the scan passes it, so a full scan stays linear in the size.
struct HitCursor {
    needle: Option<usize>,
    escape: Option<usize>,
}

impl PromptPrefilter {
    pub fn new(query: &str) -> Option<Self> {
        if query.is_empty() || query.chars().any(is_regex_meta) {
            return None;
        }
        let needle = BytesRegex::new(&format!("(?iu){}", regex::escape(query))).ok()?;
        let mut query_chars: Vec<char> = query.chars().collect();
        query_chars.sort_unstable();
        query_chars.dedup();
        let alternatives: Vec<String> = query_chars
            .iter()
            .map(|c| regex::escape(c.encode_utf8(&mut [0; 4])))
            .collect();
        let query_char = BytesRegex::new(&format!("(?iu)^(?:{})$", alternatives.join("|"))).ok()?;
        // Other characters can only match through case folding, and a
        // character without case mappings folds only to itself.
        let mut matching_chars = query_chars.clone();
        if query_chars.iter().any(|&c| has_case_mapping(c)) {
            matching_chars.extend(
                (0..CASED_LIMIT)
                    .filter_map(char::from_u32)
                    .filter(|&c| has_case_mapping(c))
                    .filter(|c| query_char.is_match(c.encode_utf8(&mut [0; 4]).as_bytes())),
            );
        }
        matching_chars.sort_unstable();
        matching_chars.dedup();
        let mut prefilter = Self {
            needle,
            matching_chars,
            short_escape: [false; 256],
        };
        for (escape, decoded) in [
            (b'"', '"'),
            (b'\\', '\\'),
            (b'/', '/'),
            (b'b', '\u{8}'),
            (b'f', '\u{c}'),
            (b'n', '\n'),
            (b'r', '\r'),
            (b't', '\t'),
        ] {
            prefilter.short_escape[escape as usize] = prefilter.is_query_char(decoded);
        }
        Some(prefilter)
    }

    fn is_query_char(&self, c: char) -> bool {
        self.matching_chars.binary_search(&c).is_ok()
    }

    pub fn may_match(&self, raw: &[u8]) -> bool {
        self.needle.is_match(raw) || self.next_escape(raw, 0).is_some()
    }

    /// The JSONL lines of `raw` that may hold a matching prompt, each
    /// followed by `\n`; `None` when no line can. Returns `raw` itself when
    /// most of it is needed anyway.
    pub fn candidate_lines<'a>(&self, raw: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        let mut cursor = HitCursor {
            needle: None,
            escape: None,
        };
        let mut ranges = Vec::new();
        let mut total = 0;
        let mut from = 0;
        while let Some(at) = self.next_hit(raw, from, &mut cursor) {
            let start = memchr::memrchr(b'\n', &raw[..at]).map_or(0, |i| i + 1);
            let end = memchr::memchr(b'\n', &raw[at..]).map_or(raw.len(), |i| at + i);
            ranges.push(start..end);
            total += end - start + 1;
            if total * 2 > raw.len() {
                return Some(Cow::Borrowed(raw));
            }
            from = end + 1;
        }
        if ranges.is_empty() {
            return None;
        }
        let mut lines = Vec::with_capacity(total);
        for range in ranges {
            lines.extend_from_slice(&raw[range]);
            lines.push(b'\n');
        }
        Some(Cow::Owned(lines))
    }

    /// Offset of the first needle match or relevant escape at or after
    /// `from`, which must be the start of a line.
    fn next_hit(&self, raw: &[u8], from: usize, cursor: &mut HitCursor) -> Option<usize> {
        if from >= raw.len() {
            return None;
        }
        if cursor.needle.is_none_or(|at| at < from) {
            cursor.needle = Some(
                self.needle
                    .find_at(raw, from)
                    .map_or(usize::MAX, |m| m.start()),
            );
        }
        if cursor.escape.is_none_or(|at| at < from) {
            cursor.escape = Some(self.next_escape(raw, from).unwrap_or(usize::MAX));
        }
        let at = cursor.needle.min(cursor.escape)?;
        (at != usize::MAX).then_some(at)
    }

    /// Offset of the first escape at or after `from` that may decode to a
    /// query character. `from` must not point into the middle of an escape.
    fn next_escape(&self, raw: &[u8], from: usize) -> Option<usize> {
        let mut i = from;
        while let Some(pos) = memchr::memchr(b'\\', &raw[i..]) {
            let at = i + pos;
            let Some(&kind) = raw.get(at + 1) else {
                return Some(at);
            };
            let (len, relevant) = if kind == b'u' {
                let cp = raw.get(at + 2..at + 6).and_then(hex4);
                // Surrogate halves and malformed escapes are not decoded:
                // assume they may produce a query character.
                match cp.and_then(char::from_u32) {
                    Some(c) => (6, self.is_query_char(c)),
                    None => (2, true),
                }
            } else {
                (2, self.short_escape[kind as usize])
            };
            if relevant {
                return Some(at);
            }
            i = at + len;
        }
        None
    }
}

/// Every character with a case mapping is below this code point (checked in
/// the tests).
const CASED_LIMIT: u32 = 0x20000;

fn hex4(digits: &[u8]) -> Option<u32> {
    digits.iter().try_fold(0, |value, &digit| {
        Some(value * 16 + char::from(digit).to_digit(16)?)
    })
}

/// Whether `c` has an upper- or lowercase mapping, i.e. may case-fold to or
/// from another character. Checked against the regex engine in the tests.
fn has_case_mapping(c: char) -> bool {
    fn only(mut mapped: impl Iterator<Item = char>, c: char) -> bool {
        mapped.next() == Some(c) && mapped.next().is_none()
    }
    !(only(c.to_lowercase(), c) && only(c.to_uppercase(), c))
}

/// Characters that make a query a regex rather than a literal. `#`, `&`,
/// `-` and `~` are literal outside character classes and the `x` flag.
fn is_regex_meta(c: char) -> bool {
    matches!(
        c,
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::find_plugin;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn test_search_fulltext_matches() {
        let plugin = find_plugin("claude").unwrap();
        let pattern = BytesRegex::new("auth").unwrap();
        assert!(search_fulltext_matches(
            &fixture_path("claude_session.jsonl"),
            plugin,
            &pattern
        ));
        assert!(!search_fulltext_matches(
            &fixture_path("claude_session.jsonl"),
            plugin,
            &BytesRegex::new("ZZZZNOTEXIST").unwrap()
        ));
    }

    #[test]
    fn test_search_prompts_matches_claude() {
        let path = fixture_path("claude_session.jsonl");
        let pattern = Regex::new("auth").unwrap();
        assert!(search_prompts_matches(
            &path,
            find_plugin("claude").unwrap(),
            &pattern,
            None
        ));
    }

    #[test]
    fn test_search_prompts_matches_codex() {
        let path = fixture_path("codex_session.jsonl");
        let pattern = Regex::new("redis").unwrap();
        assert!(search_prompts_matches(
            &path,
            find_plugin("codex").unwrap(),
            &pattern,
            None
        ));
    }

    #[test]
    fn test_search_prompts_matches_cursor() {
        let path = fixture_path("cursor_session.jsonl");
        let pattern = Regex::new("dark mode").unwrap();
        assert!(search_prompts_matches(
            &path,
            find_plugin("cursor").unwrap(),
            &pattern,
            None
        ));
    }

    #[test]
    fn test_search_prompts_matches_gemini() {
        let path = fixture_path("gemini_session.json");
        let pattern = Regex::new("review this plan").unwrap();
        assert!(search_prompts_matches(
            &path,
            find_plugin("gemini").unwrap(),
            &pattern,
            None
        ));
    }

    #[test]
    fn test_search_prompts_no_match() {
        let path = fixture_path("claude_session.jsonl");
        let pattern = Regex::new("ZZZZNOTEXIST").unwrap();
        assert!(!search_prompts_matches(
            &path,
            find_plugin("claude").unwrap(),
            &pattern,
            None
        ));
    }

    fn may_match(query: &str, raw: &str) -> bool {
        PromptPrefilter::new(query)
            .unwrap()
            .may_match(raw.as_bytes())
    }

    #[test]
    fn prompt_prefilter_only_for_literal_queries() {
        assert!(PromptPrefilter::new("auth flow").is_some());
        assert!(PromptPrefilter::new("foo-bar #1 & ~x").is_some());
        assert!(PromptPrefilter::new("修正").is_some());
        assert!(PromptPrefilter::new("").is_none());
        for regex in [
            "a.b", "^fix", "end$", "a|b", "x+", "(a)", "[ab]", "a{2}", r"\d",
        ] {
            assert!(PromptPrefilter::new(regex).is_none(), "{regex}");
        }
    }

    #[test]
    fn prompt_prefilter_verbatim_match_is_case_insensitive() {
        assert!(may_match("OAuth", r#"{"text":"fix oauth flow"}"#));
        assert!(may_match("修正", r#"{"text":"バグ修正"}"#));
        // Unicode simple case folding: KELVIN SIGN matches `k`
        assert!(may_match("k", "{\"text\":\"\u{212a}\"}"));
        assert!(!may_match("OAuth", r#"{"text":"fix login flow"}"#));
    }

    #[test]
    fn prompt_prefilter_ignores_escapes_that_cannot_match() {
        // `\n`, `\"`, `\\`, a control character and CJK written as `\uXXXX`
        // cannot decode to a character of these queries
        let raw = r#"{"text":"line\none \"quoted\" C:\\dir \u001b[0m \u30d0\u30b0"}"#;
        assert!(!may_match("error", raw));
        assert!(!may_match("修正", raw));
    }

    #[test]
    fn prompt_prefilter_keeps_sessions_with_matching_escapes() {
        // non-ASCII written as `\uXXXX`
        assert!(may_match("修正", r#"{"text":"\u4fee\u6b63"}"#));
        // an escaped ASCII letter, and one that only folds to the query
        assert!(may_match("abc", r#"{"text":"\u0041bc"}"#));
        assert!(may_match("k", r#"{"text":"\u212a"}"#));
        // short escapes of characters in the query
        assert!(may_match(r#"say "hi""#, r#"{"text":"say \"hi\""}"#));
        assert!(may_match("src/main", r#"{"text":"src\/main"}"#));
        // `<` escaped by Go's encoding/json
        assert!(may_match("<b>", r#"{"text":"\u003cb\u003e"}"#));
        // surrogate halves and malformed escapes are kept conservatively
        assert!(may_match("x", r#"{"text":"\ud83d\ude00"}"#));
        assert!(may_match("x", r#"{"text":"\u12"}"#));
        assert!(may_match("x", "{\"text\":\"trailing\\"));
    }

    /// `PromptPrefilter::new` looks for case-insensitive equivalents only
    /// among characters with case mappings below `CASED_LIMIT`; no other
    /// character may be case-insensitively equal to a different one.
    #[test]
    fn characters_without_case_mapping_fold_only_to_themselves() {
        assert!(
            (CASED_LIMIT..=0x10FFFF)
                .filter_map(char::from_u32)
                .all(|c| !has_case_mapping(c))
        );
        let cased: String = (0..=0x10FFFF)
            .filter_map(char::from_u32)
            .filter(|&c| has_case_mapping(c))
            .map(|c| regex::escape(c.encode_utf8(&mut [0; 4])))
            .collect::<Vec<_>>()
            .join("|");
        let any_cased = BytesRegex::new(&format!("(?iu)^(?:{cased})$")).unwrap();
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            if !has_case_mapping(c) {
                let bytes = c.encode_utf8(&mut [0; 4]).as_bytes().to_vec();
                assert!(!any_cased.is_match(&bytes), "U+{:04X}", c as u32);
            }
        }
    }

    #[test]
    fn prompt_prefilter_candidate_lines() {
        let prefilter = PromptPrefilter::new("auth").unwrap();
        let other = r#"{"a":"an unrelated line"}"#;
        let raw = [
            other,
            r#"{"a":"fix AUTH"}"#,
            other,
            other,
            r#"{"a":"\u0061uth auth"}"#,
            other,
        ]
        .join("\n");
        assert_eq!(
            prefilter.candidate_lines(raw.as_bytes()).unwrap().as_ref(),
            [r#"{"a":"fix AUTH"}"#, r#"{"a":"\u0061uth auth"}"#, ""]
                .join("\n")
                .as_bytes()
        );
        assert!(prefilter.candidate_lines(other.as_bytes()).is_none());
        // most of the file is needed: the raw bytes are returned uncopied
        let dense = [r#"{"a":"auth"}"#, r#"{"a":"auth"}"#, other].join("\n");
        assert!(matches!(
            prefilter.candidate_lines(dense.as_bytes()),
            Some(Cow::Borrowed(_))
        ));
        // a single huge line with many hits is found once
        let huge = format!("{other}\n{{\"a\":\"{}\"}}", "auth".repeat(100_000));
        let lines = prefilter.candidate_lines(huge.as_bytes()).unwrap();
        assert_eq!(lines.len(), huge.len());
    }

    fn fixture_sessions(tmp: &Path) -> Vec<(&'static str, PathBuf)> {
        let mut cases: Vec<(&'static str, PathBuf)> = [
            ("claude", "claude_session.jsonl"),
            ("claude", "claude_user_array.jsonl"),
            ("claude", "claude_session_headers.jsonl"),
            ("codex", "codex_session.jsonl"),
            ("cursor", "cursor_session.jsonl"),
            ("cursor", "cursor_user_query.jsonl"),
            ("gemini", "gemini_session.json"),
            ("gemini", "gemini_session.jsonl"),
        ]
        .into_iter()
        .map(|(agent, name)| (agent, fixture_path(name)))
        .collect();
        let copilot = tmp.join(".copilot/session-state/cp-named");
        std::fs::create_dir_all(&copilot).unwrap();
        std::fs::copy(
            fixture_path("copilot_workspace.yaml"),
            copilot.join("workspace.yaml"),
        )
        .unwrap();
        std::fs::copy(
            fixture_path("copilot_events.jsonl"),
            copilot.join("events.jsonl"),
        )
        .unwrap();
        cases.push(("copilot", copilot.join("workspace.yaml")));
        let grok = tmp.join("grok.jsonl");
        std::fs::write(
            &grok,
            [
                r#"{"type":"system","content":"You are Grok"}"#,
                r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nfix the auth \u4fee\u6b63\n</user_query>"}]}"#,
                r#"{"type":"assistant","content":"the redis cache"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        cases.push(("grok", grok));
        let agy = tmp.join("agy.jsonl");
        std::fs::write(
            &agy,
            [
                r#"{"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<USER_REQUEST>\nreview this plan\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nthe auth time\n</ADDITIONAL_METADATA>"}"#,
                r#"{"source":"MODEL","type":"PLANNER_RESPONSE","content":"dark mode done"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        cases.push(("agy", agy));
        cases
    }

    /// The prefilter never rejects a session that the prompt search accepts.
    #[test]
    fn prompt_prefilter_has_no_false_negatives_on_fixtures() {
        let tmp = tempfile::tempdir().unwrap();
        let queries = [
            "auth",
            "AUTH",
            "redis",
            "dark mode",
            "review this plan",
            "parser",
            "the",
            "a",
            "e",
            "-",
            "#",
            "修正",
            "ZZZZNOTEXIST",
        ];
        for (agent, path) in fixture_sessions(tmp.path()) {
            let name = path.display();
            let plugin = find_plugin(agent).unwrap();
            assert!(plugin.prompts_in_session_json(), "{agent}");
            let raw = plugin.session_bytes(&path).unwrap();
            let is_session_file = plugin.search_path(&path) == path;
            for query in queries {
                let pattern = Regex::new(&format!("(?i){query}")).unwrap();
                let prefilter = PromptPrefilter::new(query).unwrap();
                let expected = search_prompts_matches(&path, plugin, &pattern, None);
                if expected {
                    assert!(prefilter.may_match(&raw), "{name}: {query}");
                }
                if plugin.prompts_per_jsonl_line() {
                    assert!(is_session_file, "{name}");
                    let found = prefilter.candidate_lines(&raw).is_some_and(|lines| {
                        search_prompts_matches(&path, plugin, &pattern, Some(&lines))
                    });
                    assert_eq!(found, expected, "{name} lines: {query}");
                }
                if is_session_file {
                    assert_eq!(
                        search_prompts_matches(&path, plugin, &pattern, Some(&raw)),
                        expected,
                        "{name}: {query}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_search_nonexistent_file() {
        let plugin = find_plugin("claude").unwrap();
        let pattern = BytesRegex::new("test").unwrap();
        assert!(!search_fulltext_matches(
            Path::new("/nonexistent/file"),
            plugin,
            &pattern
        ));
    }
}

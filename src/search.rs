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
/// escape (`\"`, `\n`, `é`, ...). `may_match` therefore returns `false`
/// only when the raw bytes lack the query and no escape decodes to a
/// character that the query can match case-insensitively.
pub struct PromptPrefilter {
    needle: BytesRegex,
    /// Matches exactly one character that equals a query character under the
    /// same case-insensitive rules as the prompt regex.
    query_char: BytesRegex,
    /// Indexed by the byte after `\`: whether that short escape can decode to
    /// a query character. `u` is handled separately.
    short_escape: [bool; 256],
}

impl PromptPrefilter {
    pub fn new(query: &str) -> Option<Self> {
        if query.is_empty() || query.chars().any(is_regex_meta) {
            return None;
        }
        let needle = BytesRegex::new(&format!("(?iu){}", regex::escape(query))).ok()?;
        let mut chars: Vec<char> = query.chars().collect();
        chars.sort_unstable();
        chars.dedup();
        let alternatives: Vec<String> = chars
            .iter()
            .map(|c| regex::escape(c.encode_utf8(&mut [0; 4])))
            .collect();
        let query_char = BytesRegex::new(&format!("(?iu)^(?:{})$", alternatives.join("|"))).ok()?;
        let mut prefilter = Self {
            needle,
            query_char,
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
        self.query_char
            .is_match(c.encode_utf8(&mut [0; 4]).as_bytes())
    }

    pub fn may_match(&self, raw: &[u8]) -> bool {
        if self.needle.is_match(raw) {
            return true;
        }
        let mut found = false;
        self.query_char_escapes(raw, |_| {
            found = true;
            false
        });
        found
    }

    /// The JSONL lines of `raw` that may hold a matching prompt, each
    /// followed by `\n`; `None` when no line can.
    pub fn candidate_lines(&self, raw: &[u8]) -> Option<Vec<u8>> {
        let mut hits: Vec<usize> = self.needle.find_iter(raw).map(|m| m.start()).collect();
        self.query_char_escapes(raw, |at| {
            hits.push(at);
            true
        });
        if hits.is_empty() {
            return None;
        }
        hits.sort_unstable();
        let mut lines = Vec::new();
        let mut next_line = 0;
        for at in hits {
            if at < next_line {
                continue;
            }
            let start = memchr::memrchr(b'\n', &raw[..at]).map_or(0, |i| i + 1);
            let end = memchr::memchr(b'\n', &raw[at..]).map_or(raw.len(), |i| at + i);
            lines.extend_from_slice(&raw[start..end]);
            lines.push(b'\n');
            next_line = end + 1;
        }
        Some(lines)
    }

    /// Visit the offset of each escape that may decode to a query character,
    /// until `visit` returns `false`.
    fn query_char_escapes(&self, raw: &[u8], mut visit: impl FnMut(usize) -> bool) {
        let mut i = 0;
        while let Some(pos) = memchr::memchr(b'\\', &raw[i..]) {
            let at = i + pos;
            let Some(&kind) = raw.get(at + 1) else {
                visit(at);
                return;
            };
            let (len, relevant) = if kind == b'u' {
                let cp = raw
                    .get(at + 2..at + 6)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok());
                // Surrogate halves and malformed escapes are not decoded:
                // assume they may produce a query character.
                match cp.and_then(char::from_u32) {
                    Some(c) => (6, self.is_query_char(c)),
                    None => (2, true),
                }
            } else {
                (2, self.short_escape[kind as usize])
            };
            if relevant && !visit(at) {
                return;
            }
            i = at + len;
        }
    }
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
        // `\n`, `\"`, `\\` and control-character `\u` escapes cannot decode to
        // a character of these queries
        let raw = r#"{"text":"line\none \"quoted\" C:\\dir \u001b[0m"}"#;
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

    #[test]
    fn prompt_prefilter_candidate_lines() {
        let prefilter = PromptPrefilter::new("auth").unwrap();
        let raw = concat!(
            r#"{"a":"x"}"#,
            "\n",
            r#"{"a":"fix AUTH"}"#,
            "\n",
            r#"{"a":"y"}"#,
            "\n",
            r#"{"a":"auth auth"}"#,
        );
        assert_eq!(
            prefilter.candidate_lines(raw.as_bytes()).unwrap(),
            concat!(r#"{"a":"fix AUTH"}"#, "\n", r#"{"a":"auth auth"}"#, "\n").as_bytes()
        );
        assert!(prefilter.candidate_lines(br#"{"a":"x"}"#).is_none());
    }

    /// The prefilter never rejects a session that the prompt search accepts.
    #[test]
    fn prompt_prefilter_has_no_false_negatives_on_fixtures() {
        let cases = [
            ("claude", "claude_session.jsonl"),
            ("claude", "claude_user_array.jsonl"),
            ("claude", "claude_session_headers.jsonl"),
            ("codex", "codex_session.jsonl"),
            ("cursor", "cursor_session.jsonl"),
            ("cursor", "cursor_user_query.jsonl"),
            ("gemini", "gemini_session.json"),
            ("gemini", "gemini_session.jsonl"),
        ];
        let queries = [
            "auth",
            "AUTH",
            "redis",
            "dark mode",
            "review this plan",
            "the",
            "a",
            "e",
            "-",
            "#",
            "修正",
            "ZZZZNOTEXIST",
        ];
        for (agent, name) in cases {
            let path = fixture_path(name);
            let plugin = find_plugin(agent).unwrap();
            assert!(plugin.prompts_in_session_json());
            let raw = std::fs::read(&path).unwrap();
            for query in queries {
                let pattern = Regex::new(&format!("(?i){query}")).unwrap();
                let prefilter = PromptPrefilter::new(query).unwrap();
                let expected = search_prompts_matches(&path, plugin, &pattern, None);
                if expected {
                    assert!(prefilter.may_match(&raw), "{name}: {query}");
                }
                if plugin.prompts_per_jsonl_line() {
                    let found = prefilter.candidate_lines(&raw).is_some_and(|lines| {
                        search_prompts_matches(&path, plugin, &pattern, Some(&lines))
                    });
                    assert_eq!(found, expected, "{name} lines: {query}");
                }
                assert_eq!(
                    search_prompts_matches(&path, plugin, &pattern, Some(&raw)),
                    expected,
                    "{name}: {query}"
                );
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

use std::borrow::Cow;
use std::path::Path;

use regex::Regex;
use regex::bytes::Regex as BytesRegex;

use crate::agents::AgentPlugin;
use crate::agents::{Message, MessageRole};
use crate::resolver::extract_match_context;

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

/// Prompt-only search through the plugin-provided user messages. Returns
/// the match context of the first matching prompt, or `None` when none
/// matches. `data` is the session file's content when already loaded.
pub fn search_prompts(
    path: &Path,
    plugin: &dyn AgentPlugin,
    pattern: &Regex,
    data: Option<&[u8]>,
) -> Option<String> {
    let mut found = None;
    let mut visit = |message: Message| {
        if message.role != MessageRole::User {
            return true;
        }
        match pattern.find(&message.text) {
            Some(m) => {
                found = Some(extract_match_context(&message.text, m.start(), m.end(), 30));
                false
            }
            None => true,
        }
    };
    match data {
        Some(data) => plugin.iter_messages_from_bytes(path, data, &mut visit),
        None => plugin.iter_messages(path, &mut visit),
    }
    found
}

/// Default full-text search through the plugin-provided search texts
/// (messages, tool calls and tool outputs). Returns the match context of
/// the first matching text, or `None` when none matches.
pub fn search_texts(
    path: &Path,
    plugin: &dyn AgentPlugin,
    pattern: &Regex,
    data: Option<&[u8]>,
) -> Option<String> {
    let mut found = None;
    let mut visit = |text: &str| match pattern.find(text) {
        Some(m) => {
            found = Some(extract_match_context(text, m.start(), m.end(), 30));
            false
        }
        None => true,
    };
    match data {
        Some(data) => plugin.iter_search_texts_from_bytes(path, data, &mut visit),
        None => plugin.iter_search_texts(path, &mut visit),
    }
    found
}

/// Raw-bytes check that lets message search skip JSON parsing.
///
/// Built from literals that every match must contain: the query itself when
/// it is a literal, or the prefixes the regex engine extracts from it. A
/// message is a JSON string value, so every character of a match is either
/// stored verbatim — then the raw bytes contain one of the literals — or
/// written as an escape (`\"`, `\n`, `\u00e9`, ...). `may_match` therefore
/// returns `false` only when the raw bytes lack every literal and no escape
/// decodes to a character that a literal can match case-insensitively.
pub struct QueryPrefilter {
    needle: BytesRegex,
    /// Sorted characters that equal a query character under the same
    /// case-insensitive rules as the prompt regex.
    matching_chars: Vec<char>,
    /// Indexed by the byte after `\`: whether that short escape can decode to
    /// a query character. `u` is handled separately.
    short_escape: [bool; 256],
}

/// Position of the next needle match at or after the scan offset
/// (`usize::MAX` when there is none), recomputed only once the scan passes
/// it. Escapes are scanned only up to that position, so a search that stops
/// at an early hit does not scan the rest of the file, and a full scan still
/// reads every byte once.
struct HitCursor {
    needle: Option<usize>,
}

impl QueryPrefilter {
    /// `None` when the query gives no literal to look for, e.g. `.*`.
    pub fn new(query: &str) -> Option<Self> {
        if query.is_empty() {
            return None;
        }
        if !query.chars().any(is_regex_meta) {
            return Self::from_literals(&[query.to_string()]);
        }
        Self::from_literals(&regex_prefixes(query)?)
    }

    fn from_literals(literals: &[String]) -> Option<Self> {
        if literals.is_empty() || literals.iter().any(|l| l.is_empty()) {
            return None;
        }
        let escaped: Vec<String> = literals.iter().map(|l| regex::escape(l)).collect();
        let needle = BytesRegex::new(&format!("(?iu){}", escaped.join("|"))).ok()?;
        let mut query_chars: Vec<char> = literals.iter().flat_map(|l| l.chars()).collect();
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
        self.needle.is_match(raw) || self.next_escape(raw, 0, raw.len()).is_some()
    }

    /// The JSONL lines of `raw` that may hold a matching prompt, each
    /// followed by `\n`; `None` when no line can. Returns `raw` itself when
    /// most of it is needed anyway.
    pub fn candidate_lines<'a>(&self, raw: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        let mut cursor = HitCursor { needle: None };
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

    /// Visit the JSONL lines of `raw` (without the trailing `\n`) that
    /// `keep` accepts and that may hold a match, in order, until `visit`
    /// returns `false`. `keep` runs first, so lines it rejects are never
    /// searched for the literals.
    pub fn for_each_candidate_line(
        &self,
        raw: &[u8],
        keep: impl Fn(&[u8]) -> bool,
        mut visit: impl FnMut(&[u8]) -> bool,
    ) {
        let mut start = 0;
        while start < raw.len() {
            let end = memchr::memchr(b'\n', &raw[start..]).map_or(raw.len(), |i| start + i);
            let line = &raw[start..end];
            if keep(line) && self.may_match(line) && !visit(line) {
                return;
            }
            start = end + 1;
        }
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
        let needle = cursor.needle?;
        let at = self.next_escape(raw, from, needle).unwrap_or(needle);
        (at != usize::MAX).then_some(at)
    }

    /// Offset of the first escape in `from..limit` that may decode to a
    /// query character. `from` must not point into the middle of an escape.
    fn next_escape(&self, raw: &[u8], from: usize, limit: usize) -> Option<usize> {
        let limit = limit.min(raw.len());
        let mut i = from;
        while let Some(pos) = raw
            .get(i..limit)
            .and_then(|rest| memchr::memchr(b'\\', rest))
        {
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

/// Per-line check applied before a candidate JSONL line is parsed: the query
/// regex run over the line with its JSON escapes decoded. A search text is
/// one JSON string value, so its decoded content appears contiguously in the
/// decoded line, and a match in it is a match in the line. Only built for
/// queries without anchors or other look-around assertions, whose matches
/// may depend on where the text starts and ends.
pub struct LineCheck {
    pattern: BytesRegex,
}

impl LineCheck {
    pub fn new(query: &str) -> Option<Self> {
        let hir = regex_syntax::parse(&format!("(?i){query}")).ok()?;
        if !hir.properties().look_set().is_empty() {
            return None;
        }
        let pattern = BytesRegex::new(&format!("(?iu){query}")).ok()?;
        Some(Self { pattern })
    }

    pub fn may_match(&self, line: &[u8]) -> bool {
        if memchr::memchr(b'\\', line).is_none() {
            return self.pattern.is_match(line);
        }
        thread_local! {
            static DECODED: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        DECODED.with_borrow_mut(|decoded| {
            decode_json_escapes(line, decoded);
            self.pattern.is_match(decoded)
        })
    }
}

/// `raw` with its JSON string escapes decoded into `out`. Malformed escapes
/// are copied as they are; unpaired surrogates become U+FFFD.
fn decode_json_escapes(raw: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut i = 0;
    while let Some(pos) = memchr::memchr(b'\\', &raw[i..]) {
        let at = i + pos;
        out.extend_from_slice(&raw[i..at]);
        let Some(&kind) = raw.get(at + 1) else {
            out.push(b'\\');
            return;
        };
        if kind != b'u' {
            out.push(match kind {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                b'b' => 0x08,
                b'f' => 0x0c,
                other => other,
            });
            i = at + 2;
            continue;
        }
        let Some(unit) = raw.get(at + 2..at + 6).and_then(hex4) else {
            out.extend_from_slice(&raw[at..at + 2]);
            i = at + 2;
            continue;
        };
        i = at + 6;
        let low = if (0xD800..0xDC00).contains(&unit) && raw.get(i..i + 2) == Some(b"\\u") {
            raw.get(i + 2..i + 6)
                .and_then(hex4)
                .filter(|low| (0xDC00..0xE000).contains(low))
        } else {
            None
        };
        let code = match low {
            Some(low) => {
                i += 6;
                0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
            }
            None => unit,
        };
        let c = char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER);
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
    out.extend_from_slice(&raw[i..]);
}

/// Literals one of which starts every match of `query`, extracted from the
/// pattern under the same `(?i)` flag as the search (class intersections
/// and inline flags can depend on it), then compared case-insensitively.
/// `None` when the set is unbounded or a match may start with an empty
/// literal.
fn regex_prefixes(query: &str) -> Option<Vec<String>> {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    let hir = regex_syntax::parse(&format!("(?i){query}")).ok()?;
    let mut extractor = Extractor::new();
    extractor.kind(ExtractKind::Prefix);
    let seq = extractor.extract(&hir);
    let literals: Vec<String> = seq
        .literals()?
        .iter()
        .map(|lit| {
            let text = std::str::from_utf8(lit.as_bytes()).ok()?;
            (!text.is_empty()).then(|| text.to_string())
        })
        .collect::<Option<_>>()?;
    Some(dedup_case_variants(literals))
}

/// Drop literals that a kept literal already matches case-insensitively:
/// the `(?i)` parse spells out every case variant (`redis`, `Redis`, ...),
/// and the case-insensitive needle needs only one of them.
fn dedup_case_variants(literals: Vec<String>) -> Vec<String> {
    let mut kept: Vec<(String, Option<BytesRegex>)> = Vec::new();
    for literal in literals {
        let covered = kept.iter().any(|(_, re)| {
            re.as_ref()
                .is_some_and(|re| re.is_match(literal.as_bytes()))
        });
        if !covered {
            let re = BytesRegex::new(&format!("(?iu)^{}$", regex::escape(&literal))).ok();
            kept.push((literal, re));
        }
    }
    kept.into_iter().map(|(literal, _)| literal).collect()
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

    fn prompt_matches(
        path: &Path,
        plugin: &dyn AgentPlugin,
        pattern: &Regex,
        data: Option<&[u8]>,
    ) -> bool {
        search_prompts(path, plugin, pattern, data).is_some()
    }

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
        assert!(prompt_matches(
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
        assert!(prompt_matches(
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
        assert!(prompt_matches(
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
        assert!(prompt_matches(
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
        assert!(!prompt_matches(
            &path,
            find_plugin("claude").unwrap(),
            &pattern,
            None
        ));
    }

    fn may_match(query: &str, raw: &str) -> bool {
        QueryPrefilter::new(query)
            .unwrap()
            .may_match(raw.as_bytes())
    }

    #[test]
    fn prefilter_needs_a_literal_to_look_for() {
        assert!(QueryPrefilter::new("auth flow").is_some());
        assert!(QueryPrefilter::new("foo-bar #1 & ~x").is_some());
        assert!(QueryPrefilter::new("修正").is_some());
        for regex in ["a.b", "^fix", "end$", "a|b", "x+", "(a)", "[ab]", "a{2}"] {
            assert!(QueryPrefilter::new(regex).is_some(), "{regex}");
        }
        for query in ["", ".*", r"\w+", "a|", "x?", r"\d"] {
            assert!(QueryPrefilter::new(query).is_none(), "{query}");
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

    /// `QueryPrefilter::new` looks for case-insensitive equivalents only
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
        let prefilter = QueryPrefilter::new("auth").unwrap();
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
        let write = |name: &str, lines: &[&str]| {
            let path = tmp.join(name);
            std::fs::write(&path, lines.join("\n")).unwrap();
            path
        };
        cases.push((
            "claude",
            write(
                "claude_tools.jsonl",
                &[
                    r#"{"type":"user","message":{"role":"user","content":"run the tests"}}"#,
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"running"},{"type":"tool_use","id":"toolu_secret_id","name":"Bash","input":{"command":"cargo test tool-arg-needle"}}]}}"#,
                    r#"{"type":"user","message":{"content":[{"tool_use_id":"toolu_secret_id","type":"tool_result","content":"ok tool-out-needle"}]}}"#,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_x","content":[{"type":"text","text":"done"}]}]}}"#,
                    r#"{"type":"assistant","message":{"content":[{"type":"web_search_tool_result","tool_use_id":"s2","content":[{"type":"web_search_result","url":"u","encrypted_content":"ENCRYPTEDBLOB"}]}]}}"#,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_y","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAA"}}]}]}}"#,
                ],
            ),
        ));
        cases.push((
            "codex",
            write(
                "codex_tools.jsonl",
                &[
                    r#"{"type":"session_meta","payload":{"id":"s1","base_instructions":{"text":"injected-instructions"}}}"#,
                    r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"injected-instructions"}]}}"#,
                    r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the auth \u4fee\u6b63"}]}}"#,
                    r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"grep tool-arg-needle\"}","call_id":"call_id_1"}}"#,
                    r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_id_1","output":"tool-out-needle: No such file"}}"#,
                    r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c2","output":[{"type":"input_text","text":"dark mode done"}]}}"#,
                ],
            ),
        ));
        cases.push((
            "cursor",
            write(
                "cursor_tools.jsonl",
                &[
                    r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfix the parser\n</user_query>"}]}}"#,
                    r#"{"role":"assistant","message":{"content":[{"type":"text","text":"ok"},{"type":"tool_use","name":"Shell","input":{"command":"ls tool-arg-needle tool-out-needle"}}]}}"#,
                ],
            ),
        ));
        cases.push((
            "gemini",
            write(
                "session-2026-06-11T02-44-tools.jsonl",
                &[
                    r#"{"sessionId":"g-tools","projectHash":"x","startTime":"2026-06-11T02:44:54.529Z","lastUpdated":"2026-06-11T02:44:54.529Z","kind":"main"}"#,
                    r#"{"id":"m1","type":"user","content":[{"text":"use redis"}]}"#,
                    r#"{"id":"m2","type":"gemini","content":"","toolCalls":[{"id":"call_id_g","name":"run_shell_command","args":{"command":"cat tool-arg-needle"},"result":[{"functionResponse":{"id":"call_id_g","name":"run_shell_command","response":{"output":"tool-out-needle","content":[{"type":"image","source":{"data":"iVBORw0KGgoAAAA"}},{"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgoBBBB"}}]}}}]}]}"#,
                ],
            ),
        ));
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
        cases.push((
            "grok",
            write(
                "grok_tools.jsonl",
                &[
                    r#"{"type":"assistant","content":"","tool_calls":[{"id":"call_id_x","name":"run_terminal_cmd","arguments":"{\"command\":\"echo tool-arg-needle\"}"}]}"#,
                    r#"{"type":"tool_result","tool_call_id":"call_id_x","content":"tool-out-needle"}"#,
                ],
            ),
        ));
        cases.push((
            "agy",
            write(
                "agy_tools.jsonl",
                &[
                    r#"{"source":"MODEL","type":"PLANNER_RESPONSE","tool_calls":[{"name":"view_file","args":{"AbsolutePath":"\"/tmp/tool-arg-needle\""}}]}"#,
                    r#"{"source":"MODEL","type":"GENERIC","content":"File Path: tool-out-needle"}"#,
                    r#"{"source":"SYSTEM","type":"SYSTEM_MESSAGE","content":"injected-instructions"}"#,
                ],
            ),
        ));
        cases
    }

    /// The prefilter never rejects a session that the message search accepts,
    /// for prompt-only and full-text searches and for literal and regex queries.
    #[test]
    fn prefilter_has_no_false_negatives_on_fixtures() {
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
            "done",
            "ZZZZNOTEXIST",
            "tool-arg-needle",
            "tool-out-needle",
            "No such file",
            "call_id",
            "au.h",
            r"dark\s+mode",
            "(redis|parser)",
            "^fix",
            "修正|auth",
            "[a-z]+ mode",
            r"\bdone\b",
            ".*",
        ];
        for (agent, path) in fixture_sessions(tmp.path()) {
            let name = path.display();
            let plugin = find_plugin(agent).unwrap();
            assert!(plugin.prompts_in_session_json(), "{agent}");
            let raw = plugin.session_bytes(&path).unwrap();
            let is_session_file = plugin.search_path(&path) == path;
            for query in queries {
                let pattern = Regex::new(&format!("(?i){query}")).unwrap();
                let prefilter = QueryPrefilter::new(query);

                // prompt-only search
                let expected = search_prompts(&path, plugin, &pattern, None);
                if is_session_file {
                    assert_eq!(
                        search_prompts(&path, plugin, &pattern, Some(&raw)),
                        expected,
                        "{name} prompts: {query}"
                    );
                }
                if let Some(prefilter) = &prefilter {
                    if expected.is_some() {
                        assert!(prefilter.may_match(&raw), "{name} prompts: {query}");
                    }
                    if plugin.prompts_per_jsonl_line() {
                        assert!(is_session_file, "{name}");
                        let found = prefilter.candidate_lines(&raw).and_then(|lines| {
                            search_prompts(&path, plugin, &pattern, Some(&lines))
                        });
                        assert_eq!(found, expected, "{name} prompt lines: {query}");
                    }
                }

                // default full-text search
                // search texts are read from `session_bytes`
                let expected = search_texts(&path, plugin, &pattern, None);
                assert_eq!(
                    search_texts(&path, plugin, &pattern, Some(&raw)),
                    expected,
                    "{name} texts: {query}"
                );
                let Some(prefilter) = &prefilter else {
                    continue;
                };
                if plugin.search_texts_in_session_json() && expected.is_some() {
                    assert!(prefilter.may_match(&raw), "{name} texts: {query}");
                }
                if plugin.search_texts_per_jsonl_line() {
                    let check = LineCheck::new(query);
                    let mut found = None;
                    let keep = |line: &[u8]| plugin.line_may_hold_search_texts(line);
                    prefilter.for_each_candidate_line(&raw, keep, |line| {
                        if check.as_ref().is_some_and(|c| !c.may_match(line)) {
                            return true;
                        }
                        found = search_texts(&path, plugin, &pattern, Some(line));
                        found.is_none()
                    });
                    assert_eq!(found, expected, "{name} text lines: {query}");
                }
            }
        }
    }

    /// Tool call inputs and tool outputs are searched by default; JSON keys,
    /// identifiers and injected instructions are not.
    #[test]
    fn search_texts_cover_tool_io_but_not_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let hits = |agent: &str, path: &Path, query: &str| {
            let plugin = find_plugin(agent).unwrap();
            let pattern = Regex::new(&format!("(?i){query}")).unwrap();
            search_texts(path, plugin, &pattern, None).is_some()
        };
        for (agent, path) in fixture_sessions(tmp.path()) {
            for query in ["tool-arg-needle", "tool-out-needle"] {
                if path.to_string_lossy().contains("tools") {
                    assert!(
                        hits(agent, &path, query),
                        "{agent} {}: {query}",
                        path.display()
                    );
                }
            }
            for noise in [
                "ENCRYPTEDBLOB",
                "iVBORw0KGgo",
                "tool_use_id",
                "call_id",
                "base_instructions",
                "toolu_secret_id",
                "injected-instructions",
            ] {
                assert!(
                    !hits(agent, &path, noise),
                    "{agent} {}: {noise}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn line_check_decodes_escapes_and_skips_anchored_queries() {
        let check = LineCheck::new("au.h").unwrap();
        assert!(check.may_match(br#"{"text":"OAuth"}"#));
        assert!(check.may_match(br#"{"text":"au\th"}"#));
        assert!(check.may_match(br#"{"text":"\u0041uth"}"#));
        assert!(!check.may_match(br#"{"text":"au\nno"}"#));
        let check = LineCheck::new("修正").unwrap();
        assert!(check.may_match(br#"{"text":"\u4fee\u6b63"}"#));
        assert!(check.may_match("{\"text\":\"\u{4fee}\u{6b63}\"}".as_bytes()));
        let check = LineCheck::new(r"😀x").unwrap();
        assert!(check.may_match(br#"{"text":"\ud83d\ude00X"}"#));
        for anchored in ["^fix", "end$", r"\bdone\b", r"(?m)^x"] {
            assert!(LineCheck::new(anchored).is_none(), "{anchored}");
        }
    }

    /// Search as the pipeline does it: prefilter, per-line keep and line
    /// check, then the plugin's search texts.
    fn pipeline_search(agent: &str, path: &Path, query: &str) -> Option<String> {
        let plugin = find_plugin(agent).unwrap();
        let pattern = Regex::new(&format!("(?i){query}")).unwrap();
        let raw = plugin.session_bytes(path).unwrap();
        let Some(prefilter) = QueryPrefilter::new(query) else {
            return search_texts(path, plugin, &pattern, Some(&raw));
        };
        if plugin.search_texts_per_jsonl_line() {
            let check = LineCheck::new(query);
            let keep = |line: &[u8]| plugin.line_may_hold_search_texts(line);
            let mut found = None;
            prefilter.for_each_candidate_line(&raw, keep, |line| {
                if check.as_ref().is_some_and(|c| !c.may_match(line)) {
                    return true;
                }
                found = search_texts(path, plugin, &pattern, Some(line));
                found.is_none()
            });
            return found;
        }
        if plugin.search_texts_in_session_json() && !prefilter.may_match(&raw) {
            return None;
        }
        search_texts(path, plugin, &pattern, Some(&raw))
    }

    #[test]
    fn default_search_finds_review_regressions() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |name: &str, lines: &[&str]| {
            let path = tmp.path().join(name);
            std::fs::write(&path, lines.join("\n")).unwrap();
            path
        };
        let cases = [
            // whitespace around `:` in a record
            (
                "claude",
                write(
                    "spaced.jsonl",
                    &[r#"{"type": "user", "message": {"content": "needle here"}}"#],
                ),
                "needle",
            ),
            (
                "codex",
                write(
                    "spaced_codex.jsonl",
                    &[
                        r#"{"timestamp": "t", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "needle here"}]}}"#,
                    ],
                ),
                "needle",
            ),
            // tool argument keys that are also part-level opaque keys
            (
                "claude",
                write(
                    "args.jsonl",
                    &[
                        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Move","input":{"source":"unique.txt","destination":"target.txt"}}]}}"#,
                    ],
                ),
                "unique",
            ),
            // later text blocks of a message
            (
                "claude",
                write(
                    "blocks.jsonl",
                    &[
                        r#"{"type":"user","message":{"content":[{"type":"text","text":"first"},{"type":"text","text":"needle here"}]}}"#,
                    ],
                ),
                "needle",
            ),
            (
                "cursor",
                write(
                    "cursor_blocks.jsonl",
                    &[
                        r#"{"role":"assistant","message":{"content":[{"type":"text","text":"first"},{"type":"text","text":"needle here"}]}}"#,
                    ],
                ),
                "needle",
            ),
            // tool results in a Gemini user turn
            (
                "gemini",
                write(
                    "session-2026-06-11T02-44-fr.jsonl",
                    &[
                        r#"{"sessionId":"g-fr","projectHash":"x","startTime":"2026-06-11T02:44:54.529Z","lastUpdated":"2026-06-11T02:44:54.529Z","kind":"main"}"#,
                        r#"{"id":"m1","type":"user","content":[{"functionResponse":{"id":"x1","name":"read_file","response":{"output":"needle here"}}}]}"#,
                    ],
                ),
                "needle",
            ),
            // record types written with escapes
            (
                "claude",
                write(
                    "escaped_type.jsonl",
                    &[r#"{"type":"\u0075ser","message":{"content":"needle here"}}"#],
                ),
                "needle",
            ),
            (
                "codex",
                write(
                    "escaped_type_codex.jsonl",
                    &[
                        r#"{"timestamp":"t","type":"response\u005fitem","payload":{"type":"function_call_output","output":"needle here"}}"#,
                    ],
                ),
                "needle",
            ),
            // server-tool results
            (
                "claude",
                write(
                    "server_tool.jsonl",
                    &[
                        r#"{"type":"assistant","message":{"content":[{"type":"server_tool_use","id":"s1","name":"web_search","input":{"query":"q"}},{"type":"web_search_tool_result","tool_use_id":"s1","content":[{"type":"web_search_result","url":"https://example.com/needle-page","title":"t","encrypted_content":"ENCRYPTEDBLOB"}]}]}}"#,
                    ],
                ),
                "needle-page",
            ),
            // roles inside a tool's output are not the payload's role
            (
                "codex",
                write(
                    "nested_role.jsonl",
                    &[
                        r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":[{"role":"developer","value":"needle here"}]}}"#,
                        r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call_output","call_id":"c2","output":[{"role":"user","text":"<x> other needle"}]}}"#,
                    ],
                ),
                "needle",
            ),
            // a tool's own data with a `type` field is not a content part
            (
                "claude",
                write(
                    "typed_output.jsonl",
                    &[
                        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"file","source":"unique.txt","content":"ok"}]}]}}"#,
                    ],
                ),
                "unique",
            ),
            // prefixes follow the search's case-insensitive flag
            (
                "claude",
                write(
                    "flags.jsonl",
                    &[r#"{"type":"user","message":{"content":"afoo"}}"#],
                ),
                "[a&&A]foo|bar",
            ),
        ];
        for (agent, path, query) in cases {
            assert!(
                pipeline_search(agent, &path, query).is_some(),
                "{agent} {}: {query}",
                path.display()
            );
        }
    }

    #[test]
    fn case_variant_prefixes_are_deduplicated() {
        let literals = regex_prefixes("redis|postgres").unwrap();
        assert_eq!(literals.len(), 2, "{literals:?}");
        assert!(dedup_case_variants(vec!["ab".into(), "AB".into(), "ac".into()]).len() == 2);
    }

    #[test]
    fn regex_queries_build_prefilters_from_prefixes() {
        assert!(QueryPrefilter::new(".*").is_none());
        assert!(QueryPrefilter::new("a|").is_none());
        let prefilter = QueryPrefilter::new(r"oauth\s+flow|redis").unwrap();
        assert!(prefilter.may_match(br#"{"text":"use REDIS"}"#));
        assert!(prefilter.may_match(br#"{"text":"the OAuth flow"}"#));
        assert!(!prefilter.may_match(br#"{"text":"nothing here"}"#));
        // an escape that may decode to a literal character keeps the session
        assert!(prefilter.may_match(br#"{"text":"\u0052EDIS"}"#));
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

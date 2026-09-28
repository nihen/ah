//! Occurrence-oriented search. The session pipeline remains an early-exit
//! filter for log/show/resume; only this command enumerates all occurrences.
use std::io::IsTerminal;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::agents::{self, AgentPlugin, MessageRole};
use crate::cli::{Field, FilterArgs, MatchSearchArgs, SortOrder};
use crate::color::{self, BOLD, BOLD_YELLOW, CYAN, DIM};
use crate::output::escape_tsv_lossless;
use crate::pipeline::{self, PipelineParams};
use crate::resolver::ResolveOpts;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SearchHit {
    pub path: String,
    pub agent: String,
    pub project: String,
    pub id: String,
    pub modified_at: String,
    pub title: String,
    pub text_index: usize,
    pub match_start: usize,
    pub match_end: usize,
    pub snippet: String,
    // Private SSH transport metadata, omitted from ordinary JSON/TSV output.
    // Keep the original occurrence, rather than re-matching a truncated snippet
    // (which would break anchors, long matches and repeated words).
    #[serde(default, skip_serializing, rename = "_snippet_match")]
    pub snippet_match: Option<[usize; 2]>,
}

pub fn run(args: MatchSearchArgs, mut filter: FilterArgs) -> Result<(), String> {
    if filter.raw_search {
        return Err(
            "search does not support --raw-search; use `ah log -q ... --raw-search` instead."
                .into(),
        );
    }
    if args.pattern.is_some() && filter.query.is_some() {
        return Err("Use either PATTERN or -q REGEX, not both".into());
    }
    let query = args
        .pattern
        .as_ref()
        .or(filter.query.as_ref())
        .filter(|q| !q.is_empty())
        .ok_or("search requires a non-empty PATTERN or -q REGEX")?
        .clone();
    let pattern = pipeline::compile_text_regex(&query)?;
    filter.query = Some(query.clone());
    // Validate remotes before doing local work. A failed/old remote is an error,
    // never a silently incomplete successful search.
    crate::remote::resolve_remotes(&filter.remote)?;
    let params = PipelineParams {
        resolve_fields: vec![
            Field::Path,
            Field::Agent,
            Field::Project,
            Field::Id,
            Field::ModifiedAt,
            Field::Title,
        ],
        resolve_opts: ResolveOpts::new(&query, 500, 50).with_search_mode(filter.search_mode()),
        filters: filter.to_filters(),
        since: filter.since_time()?,
        until: filter.until_time()?,
        query,
        search_mode: filter.search_mode(),
        sort_field: Field::ModifiedAt,
        sort_order: SortOrder::Desc,
        collect_limit: filter.limit,
        running: filter.running,
        require_resume_cmd: false,
    };
    let mut sessions = pipeline::run_pipeline(&params)?.sessions;
    sessions.sort_by(|a, b| {
        b.fields
            .get(&Field::ModifiedAt)
            .cmp(&a.fields.get(&Field::ModifiedAt))
            .then_with(|| a.fields.get(&Field::Path).cmp(&b.fields.get(&Field::Path)))
    });
    let mut hits = Vec::new();
    for session in sessions {
        let remaining = if args.max_matches == 0 {
            usize::MAX
        } else {
            args.max_matches.saturating_sub(hits.len())
        };
        if remaining == 0 {
            break;
        }
        let plugin = agents::find_plugin_for_path(&session.path);
        visit_occurrences(
            &session.path,
            plugin,
            &pattern,
            filter.prompt_only,
            remaining,
            &mut |index, start, end, text| {
                let field = |f| session.fields.get(&f).cloned().unwrap_or_default();
                let (snippet, snippet_match) =
                    snippet(text, start, end, args.snippet_length as usize);
                hits.push(SearchHit {
                    path: field(Field::Path),
                    agent: field(Field::Agent),
                    project: field(Field::Project),
                    id: field(Field::Id),
                    modified_at: field(Field::ModifiedAt),
                    title: field(Field::Title),
                    text_index: index,
                    match_start: start,
                    match_end: end,
                    snippet,
                    snippet_match,
                });
            },
        );
    }
    hits.extend(crate::remote::fetch_search_hits(&args, &filter)?);
    // Stable sorting preserves source order inside each session. Paths also
    // distinguish sessions with equal ids on different hosts.
    hits.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.text_index.cmp(&b.text_index))
            .then_with(|| a.match_start.cmp(&b.match_start))
    });
    if args.max_matches != 0 {
        hits.truncate(args.max_matches);
    }
    print_hits(&hits, &args);
    Ok(())
}

/// Visit occurrences without collecting a transcript or a list of matches. Indices
/// refer to the plugin's search fragments in the selected mode; they are not
/// persistent message ids and cannot be used as `show --head` positions.
pub(crate) fn visit_occurrences(
    path: &std::path::Path,
    plugin: &dyn AgentPlugin,
    pattern: &Regex,
    prompt_only: bool,
    limit: usize,
    visit: &mut dyn FnMut(usize, usize, usize, &str),
) {
    let mut index = 0;
    let mut count = 0;
    let mut text_visitor = |text: &str| {
        index += 1;
        if text.is_empty() {
            return true;
        }
        for m in pattern.find_iter(text) {
            if count >= limit {
                return false;
            }
            visit(index, m.start(), m.end(), text);
            count += 1;
            if count >= limit {
                return false;
            }
        }
        true
    };
    if prompt_only {
        plugin.iter_messages(path, &mut |message| {
            message.role != MessageRole::User || text_visitor(&message.text)
        });
    } else {
        plugin.iter_search_texts(path, &mut text_visitor);
    }
}

/// Bounded even when the regex itself matches megabytes. UTF-8 boundaries are
/// preserved. The Unicode ellipses are included in the character budget.
fn snippet(text: &str, start: usize, end: usize, limit: usize) -> (String, Option<[usize; 2]>) {
    let match_chars = text[start..end].chars().take(limit).count();
    let before = limit.saturating_sub(match_chars) / 2;
    let from = text[..start]
        .char_indices()
        .rev()
        .take(before)
        .last()
        .map_or(start, |(i, _)| i);
    let prefix = usize::from(from > 0);
    let available = limit.saturating_sub(prefix);
    let mut to = text[from..]
        .char_indices()
        .nth(available)
        .map_or(text.len(), |(i, _)| from + i);
    if to < text.len() && available > 0 {
        to = text[from..to]
            .char_indices()
            .next_back()
            .map_or(from, |(i, _)| from + i);
    }
    let mut result = String::new();
    if prefix > 0 {
        result.push('…');
    }
    result.push_str(&text[from..to]);
    if to < text.len() && result.chars().count() < limit {
        result.push('…');
    }
    let visible_end = end.min(to);
    let prefix_bytes = if from > 0 { '…'.len_utf8() } else { 0 };
    let matched = (start < visible_end).then(|| {
        [
            prefix_bytes + start - from,
            prefix_bytes + visible_end - from,
        ]
    });
    (result, matched)
}

fn print_hits(hits: &[SearchHit], args: &MatchSearchArgs) {
    let pretty =
        !args.json && !args.tsv && (crate::pager::is_active() || std::io::stdout().is_terminal());
    let mut previous_path = None;
    for hit in hits {
        if args.json {
            if args.search_wire {
                let mut value = serde_json::to_value(hit).expect("SearchHit serialization");
                value["_snippet_match"] = serde_json::json!(hit.snippet_match);
                println!("{value}");
            } else {
                println!(
                    "{}",
                    serde_json::to_string(hit).expect("SearchHit serialization")
                );
            }
        } else if pretty {
            let new_session = previous_path != Some(hit.path.as_str());
            if previous_path.is_some() && (new_session || args.verbose) {
                println!();
            }
            print!(
                "{}",
                pretty_hit(hit, new_session, color::use_color(), args.verbose)
            );
            previous_path = Some(hit.path.as_str());
        } else {
            println!(
                "{}",
                [
                    hit.path.clone(),
                    hit.agent.clone(),
                    hit.project.clone(),
                    hit.id.clone(),
                    hit.text_index.to_string(),
                    hit.match_start.to_string(),
                    hit.match_end.to_string(),
                    hit.snippet.clone()
                ]
                .iter()
                .map(|v| escape_tsv_lossless(v))
                .collect::<Vec<_>>()
                .join("\t")
            );
        }
    }
}

/// Style only sanitized terminal text. Keep escape sequences out of structured
/// output and make layout equally usable when color is disabled.
fn pretty_hit(hit: &SearchHit, new_session: bool, colored: bool, verbose: bool) -> String {
    use std::fmt::Write;
    let style = |code: &str, text: &str| {
        color::colorize(if colored { code } else { "" }, &display_text(text))
    };
    let mut out = String::new();
    if new_session {
        // Keep the source host visible even when the full log path is hidden.
        let origin = crate::remote::parse_remote_path(&hit.path)
            .map(|(remote, _)| format!("  remote:{}", remote.name))
            .unwrap_or_default();
        writeln!(
            out,
            "{}",
            style(
                &format!("{BOLD}{CYAN}"),
                &format!(
                    "session {}  [{}] {}{}",
                    hit.id, hit.agent, hit.project, origin
                )
            )
        )
        .unwrap();
        if !hit.title.is_empty() {
            writeln!(out, "  {}", style(BOLD, &hit.title)).unwrap();
        }
        writeln!(out, "  {}", style(DIM, &hit.modified_at)).unwrap();
        if verbose {
            writeln!(out, "  {}", style(DIM, &hit.path)).unwrap();
        }
        writeln!(out).unwrap();
    }
    if verbose {
        writeln!(
            out,
            "  {}",
            style(
                DIM,
                &format!(
                    "text #{}  bytes {}..{}",
                    hit.text_index, hit.match_start, hit.match_end
                )
            )
        )
        .unwrap();
    }
    writeln!(
        out,
        "    {}",
        highlighted_snippet(&hit.snippet, hit.snippet_match, colored)
    )
    .unwrap();
    out
}

fn highlighted_snippet(text: &str, span: Option<[usize; 2]>, colored: bool) -> String {
    if colored {
        // Remote data may be old or malformed. Never slice unchecked offsets
        // and never guess the match by searching a truncated excerpt again.
        if let Some([start, end]) = span {
            if start < end {
                if let (Some(before), Some(matched), Some(after)) =
                    (text.get(..start), text.get(start..end), text.get(end..))
                {
                    return format!(
                        "{}{}{}",
                        display_text(before),
                        color::colorize(BOLD_YELLOW, &display_text(matched)),
                        display_text(after)
                    );
                }
            }
        }
    }
    display_text(text)
}

// Session content is untrusted terminal input. Do not execute embedded escape
// sequences or allow newlines to masquerade as another result/header.
fn display_text(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_tracks_the_original_occurrence_and_clipped_unicode_match() {
        let (text, span) = snippet("needle NEEDLE", 7, 13, 100);
        assert_eq!(
            highlighted_snippet(&text, span, true),
            format!("needle {BOLD_YELLOW}NEEDLE{}", color::RESET)
        );
        assert_eq!(highlighted_snippet(&text, span, false), text);
        // A regex anchored at the start/end of the original text would not
        // match this ellipsis-truncated excerpt if it were searched again.
        let source = "認証あいうえお";
        let (text, span) = snippet(source, 0, source.len(), 4);
        assert_eq!(text, "認証あ…");
        assert_eq!(
            highlighted_snippet(&text, span, true),
            format!("{BOLD_YELLOW}認証あ{}…", color::RESET)
        );
        let source = "前".repeat(100) + "認証";
        let (text, span) = snippet(&source, 300, 306, 8);
        let [start, end] = span.unwrap();
        assert_eq!(&text[start..end], "認証");
        assert!(text.starts_with('…'));
        assert_eq!(snippet("abc", 0, 0, 3).1, None);
        for limit in 1..10 {
            let (text, span) = snippet(&source, 300, 306, limit);
            if let Some([start, end]) = span {
                assert!(text.get(start..end).is_some());
                assert!("認証".starts_with(&text[start..end]));
            }
        }
    }

    #[test]
    fn remote_highlight_ranges_are_checked_and_control_text_is_escaped() {
        for span in [
            None,
            Some([2, 1]),
            Some([0, 99]),
            Some([1, 3]),
            Some([0, 0]),
        ] {
            assert_eq!(highlighted_snippet("認証", span, true), "認証");
        }
        let source = "a\x1b[31m\nb";
        assert_eq!(
            highlighted_snippet(source, Some([1, 7]), true),
            format!("a{BOLD_YELLOW}\\u{{1b}}[31m\\n{}b", color::RESET)
        );
    }

    #[test]
    fn terminal_layout_and_private_wire_metadata_preserve_public_json() {
        let value = serde_json::json!({"path":"/tmp/a", "agent":"claude", "project":"app",
            "id":"one", "modified_at":"2026-09-28 12:00", "title":"Find auth",
            "text_index":1, "match_start":0, "match_end":6, "snippet":"needle",
            "_snippet_match":[0,6]});
        let hit: SearchHit = serde_json::from_value(value).unwrap();
        let colored = pretty_hit(&hit, true, true, false);
        assert!(colored.contains(&format!("{BOLD}{CYAN}session one")));
        assert!(colored.contains(&format!("{DIM}2026-09-28")));
        assert!(!colored.contains("bytes"));
        assert!(!colored.contains("/tmp/a"));
        assert!(colored.contains(&format!("\n    {BOLD_YELLOW}needle")));
        let plain = pretty_hit(&hit, true, false, false);
        assert!(!plain.contains('\x1b'));
        assert!(plain.starts_with("session one  [claude] app\n"));
        assert!(!plain.contains('·'));
        assert!(plain.contains("\n\n    needle\n"));
        let verbose = pretty_hit(&hit, true, false, true);
        assert!(verbose.contains("/tmp/a"));
        assert!(!verbose.contains('·'));
        assert!(verbose.contains("\n  text #1  bytes 0..6\n    needle\n"));
        let styled_verbose = pretty_hit(&hit, true, true, true);
        assert!(styled_verbose.contains(&format!("{DIM}text #1  bytes 0..6")));
        assert!(!pretty_hit(&hit, false, true, false).contains("session one"));
        let public = serde_json::to_value(&hit).unwrap();
        assert!(public.get("_snippet_match").is_none());
        assert_eq!(public.as_object().unwrap().len(), 10);
    }

    #[test]
    fn snippets_are_unicode_safe_and_bounded_even_for_long_matches() {
        let text = "前置き認証あいうえお後ろ";
        let start = text.find("認証").unwrap();
        for limit in 1..30 {
            for end in [start, start + "認証".len(), text.len()] {
                let (s, _) = snippet(text, start, end, limit);
                assert!(s.chars().count() <= limit, "{s:?} at {limit}");
            }
        }
        assert_eq!(snippet("needle", 0, 6, 20).0, "needle");
        assert_eq!(
            snippet(&"x".repeat(1_000_000), 0, 1_000_000, 10).0,
            "xxxxxxxxx…"
        );
        assert_eq!(display_text("a\x1b[31m\nb"), "a\\u{1b}[31m\\nb");
    }
}

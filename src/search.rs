use std::path::Path;

use regex::Regex;
#[cfg(test)]
use regex::bytes::Regex as BytesRegex;

use crate::agents::AgentPlugin;
use crate::agents::MessageRole;
#[cfg(test)]
use crate::agents::common::mmap_file;

/// Full-text search: check if file matches (early exit).
/// NOTE: Pipeline now does mmap+search directly for better mmap sharing.
/// This function is kept for tests.
#[cfg(test)]
pub fn search_fulltext_matches(
    path: &Path,
    plugin: &dyn AgentPlugin,
    pattern: &BytesRegex,
) -> bool {
    let search_path = plugin.search_path(path);
    match mmap_file(&search_path) {
        Some(mmap) => pattern.is_match(&mmap),
        None => false,
    }
}

/// Prompt-only search through the plugin-provided user messages.
pub fn search_prompts_matches(path: &Path, plugin: &dyn AgentPlugin, pattern: &Regex) -> bool {
    let mut found = false;
    plugin.iter_messages(path, &mut |message| {
        if message.role == MessageRole::User && pattern.is_match(&message.text) {
            found = true;
            false
        } else {
            true
        }
    });
    found
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
            &pattern
        ));
    }

    #[test]
    fn test_search_prompts_matches_codex() {
        let path = fixture_path("codex_session.jsonl");
        let pattern = Regex::new("redis").unwrap();
        assert!(search_prompts_matches(
            &path,
            find_plugin("codex").unwrap(),
            &pattern
        ));
    }

    #[test]
    fn test_search_prompts_matches_cursor() {
        let path = fixture_path("cursor_session.jsonl");
        let pattern = Regex::new("dark mode").unwrap();
        assert!(search_prompts_matches(
            &path,
            find_plugin("cursor").unwrap(),
            &pattern
        ));
    }

    #[test]
    fn test_search_prompts_matches_gemini() {
        let path = fixture_path("gemini_session.json");
        let pattern = Regex::new("review this plan").unwrap();
        assert!(search_prompts_matches(
            &path,
            find_plugin("gemini").unwrap(),
            &pattern
        ));
    }

    #[test]
    fn test_search_prompts_no_match() {
        let path = fixture_path("claude_session.jsonl");
        let pattern = Regex::new("ZZZZNOTEXIST").unwrap();
        assert!(!search_prompts_matches(
            &path,
            find_plugin("claude").unwrap(),
            &pattern
        ));
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

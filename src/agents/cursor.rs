use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::first_text_part;
use super::common::for_each_jsonl_value;
use super::common::tagged_user_body;

static RE_CURSOR_PROJECTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".*/projects/([^/]+)/.*").unwrap());

static DECODE_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Decode Cursor's dash-encoded directory name to an actual filesystem path.
/// e.g. "Users-you-src-github-com-org-repo" -> "/Users/you/src/github.com/org/repo"
/// Results are cached per encoded string.
fn decode_cursor_path(encoded: &str) -> Option<String> {
    if let Ok(cache) = DECODE_CACHE.lock() {
        if let Some(result) = cache.get(encoded) {
            return result.clone();
        }
    }
    let result = decode_cursor_path_inner(Path::new("/"), encoded);
    if let Ok(mut cache) = DECODE_CACHE.lock() {
        cache.insert(encoded.to_string(), result.clone());
    }
    result
}

/// Encode one path component the way Cursor does: every run of characters
/// other than ASCII letters and digits becomes a single `-`, and leading or
/// trailing dashes are dropped (so `.claude` → `claude`, `pce_flutter` →
/// `pce-flutter`).
fn encode_cursor_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Resolve `encoded` against the directories that exist under `root`.
///
/// The encoding is lossy (`/`, `.`, `_` and `-` all become `-`), so the path
/// is rebuilt by walking the filesystem: at each level, a child directory
/// whose encoded name is a dash-bounded prefix of the remaining string is
/// descended into, backtracking on dead ends. When the directory no longer
/// exists (e.g. a removed worktree), the deepest existing ancestor is kept
/// and the undecodable rest is appended as a single component so that the
/// project name stays readable.
fn decode_cursor_path_inner(root: &Path, encoded: &str) -> Option<String> {
    let encoded = encoded.trim_matches('-');
    if encoded.is_empty() {
        return None;
    }
    let mut best: (usize, PathBuf) = (0, root.to_path_buf());
    if let Some(found) = walk_encoded(root, encoded, encoded.len(), &mut best, 0) {
        // Resolve symlinks (e.g. `/home/user` → `/data/home/user`) so the
        // result matches the canonicalized cwd filter.
        let found = std::fs::canonicalize(&found).unwrap_or(found);
        return Some(found.to_string_lossy().into_owned());
    }
    let (consumed, dir) = best;
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let rest = encoded[consumed..].trim_start_matches('-');
    Some(dir.join(rest).to_string_lossy().into_owned())
}

const MAX_DECODE_DEPTH: usize = 32;

fn walk_encoded(
    dir: &Path,
    rest: &str,
    total: usize,
    best: &mut (usize, PathBuf),
    depth: usize,
) -> Option<PathBuf> {
    if depth >= MAX_DECODE_DEPTH {
        return None;
    }
    let mut candidates: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let enc = encode_cursor_component(&e.file_name().to_string_lossy());
            let matches = !enc.is_empty()
                && rest.starts_with(&enc)
                && matches!(rest.as_bytes().get(enc.len()), None | Some(b'-'));
            // `Path::is_dir` follows symlinks, so `/home` → `/data/home` counts.
            (matches && e.path().is_dir()).then(|| (enc, e.path()))
        })
        .collect();
    // Prefer the longest match (fewest components); ties broken by path for
    // deterministic output.
    candidates.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.1.cmp(&b.1)));
    for (enc, path) in candidates {
        let remaining = rest[enc.len()..].trim_start_matches('-');
        if remaining.is_empty() {
            return Some(path);
        }
        let consumed = total - remaining.len();
        if consumed > best.0 {
            *best = (consumed, path.clone());
        }
        if let Some(found) = walk_encoded(&path, remaining, total, best, depth + 1) {
            return Some(found);
        }
    }
    None
}

pub static PLUGIN: CursorPlugin = CursorPlugin;

pub struct CursorPlugin;

/// Cursor transcripts wrap real user text in `<user_query>…</user_query>`; strip for title / prompts.
fn cursor_user_body(raw: &str) -> Option<&str> {
    tagged_user_body(raw, "user_query")
}

impl AgentPlugin for CursorPlugin {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn description(&self) -> &'static str {
        "Cursor Agent"
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: cwd path encoded in session dir name)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".cursor/projects/*/agent-transcripts/*.jsonl",
            ".cursor/projects/*/agent-transcripts/*/*.jsonl",
        ]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.cursor/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        for_each_jsonl_value(path, |val| {
            match val.get("role").and_then(|v| v.as_str()) {
                Some("user") => {
                    if let Some(raw) = val.get("message").and_then(first_text_part) {
                        if let Some(text) = cursor_user_body(raw) {
                            return visit(Message::user(text.to_string()));
                        }
                    }
                }
                Some("assistant") => {
                    if let Some(text) = val.get("message").and_then(first_text_part) {
                        return visit(Message::assistant(text.to_string()));
                    }
                }
                _ => {}
            }
            true
        });
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        let path_str = path.to_string_lossy();
        let caps = RE_CURSOR_PROJECTS.captures(&path_str)?;
        decode_cursor_path(&caps[1])
    }

    fn resolve_project(&self, path: &Path, _home: &Path) -> Option<String> {
        let path_str = path.to_string_lossy();
        RE_CURSOR_PROJECTS
            .captures(&path_str)
            .and_then(|caps| decode_cursor_path(&caps[1]))
            .or_else(|| Some("?".to_string()))
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["cursor-agent".to_string(), "--resume".to_string(), id])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkdirs(root: &Path, rels: &[&str]) {
        for rel in rels {
            std::fs::create_dir_all(root.join(rel)).unwrap();
        }
    }

    fn decode(root: &Path, encoded: &str) -> String {
        decode_cursor_path_inner(root, encoded).unwrap()
    }

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        (tmp, root)
    }

    #[test]
    fn encode_component_collapses_non_alphanumerics() {
        assert_eq!(encode_cursor_component(".claude"), "claude");
        assert_eq!(encode_cursor_component("pce_flutter"), "pce-flutter");
        assert_eq!(encode_cursor_component("github.com"), "github-com");
        assert_eq!(encode_cursor_component("a--b__"), "a-b");
    }

    #[test]
    fn decode_keeps_leading_data_component() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["data/home/u/src/github.com/nihen/ah", "home"]);
        assert_eq!(
            decode(&root, "data-home-u-src-github-com-nihen-ah"),
            root.join("data/home/u/src/github.com/nihen/ah")
                .to_string_lossy()
        );
    }

    #[test]
    fn decode_restores_dots_underscores_dashes_and_hidden_dirs() {
        let (_tmp, root) = temp_root();
        let rel = "src/a-d-systems/pce_flutter/.claude/worktrees/wt-pce-flutter-pr284";
        mkdirs(&root, &[rel, "src/a"]);
        assert_eq!(
            decode(
                &root,
                "src-a-d-systems-pce-flutter-claude-worktrees-wt-pce-flutter-pr284"
            ),
            root.join(rel).to_string_lossy()
        );
    }

    #[test]
    fn decode_backtracks_from_dead_ends() {
        let (_tmp, root) = temp_root();
        // `foo-bar` is the longer match but leads nowhere; `foo/bar/baz` is right.
        mkdirs(&root, &["foo-bar", "foo/bar/baz"]);
        assert_eq!(
            decode(&root, "foo-bar-baz"),
            root.join("foo/bar/baz").to_string_lossy()
        );
    }

    #[test]
    fn decode_removed_dir_keeps_deepest_existing_ancestor() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["src/chatCX-AXA"]);
        assert_eq!(
            decode(&root, "src-chatCX-AXA-worktrees-4297-chat-scroll"),
            root.join("src/chatCX-AXA/worktrees-4297-chat-scroll")
                .to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn decode_resolves_symlinked_prefix() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["data/home/u/proj"]);
        std::os::unix::fs::symlink(root.join("data/home"), root.join("home")).unwrap();
        assert_eq!(
            decode(&root, "home-u-proj"),
            root.join("data/home/u/proj").to_string_lossy()
        );
    }

    #[test]
    fn resume_args_are_interactive() {
        let args = PLUGIN
            .resume_args(
                Path::new("/h/.cursor/projects/p/agent-transcripts/abc/abc.jsonl"),
                Path::new("/h"),
            )
            .unwrap();
        assert_eq!(args, ["cursor-agent", "--resume", "abc"]);
    }
}

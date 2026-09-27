use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::first_text_part;
use super::common::for_each_jsonl_value_bytes;
use super::common::is_safe_cli_id;
use super::common::mmap_file;
use super::common::tagged_user_body;
use super::{MemoryKind, MemorySource};

/// Parent chat id of a subagent transcript
/// (`agent-transcripts/<parent id>/subagents/<id>.jsonl`).
fn subagent_parent(path: &Path) -> Option<&str> {
    let dir = path.parent()?;
    if dir.file_name()? != "subagents" {
        return None;
    }
    let parent = dir.parent()?;
    if parent.parent()?.file_name()? != "agent-transcripts" {
        return None;
    }
    parent.file_name()?.to_str()
}

static RE_CURSOR_PROJECTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".*/projects/([^/]+)/.*").unwrap());

static DECODE_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Child names per directory, shared by every decode in this process so that
/// common ancestors (`/`, the home directory, ...) are listed only once.
/// `None` records a directory that could not be listed.
type DirListing = Option<Arc<Vec<OsString>>>;
static LISTING_CACHE: LazyLock<Mutex<HashMap<PathBuf, DirListing>>> =
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
/// `pce-flutter`, `日本語` → ``).
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
/// descended into, backtracking on dead ends. The longest match is tried
/// first, so when both `foo-bar` and `foo/bar` exist as complete matches,
/// `foo-bar` wins; the encoding cannot tell them apart.
///
/// When the directory no longer exists (e.g. a removed worktree), the deepest
/// existing ancestor is kept and the undecodable rest is appended as a single
/// component so that the project name stays readable. When not even the first
/// component exists (a tree from another machine), every `-` is read as `/`.
fn decode_cursor_path_inner(root: &Path, encoded: &str) -> Option<String> {
    let encoded = encoded.trim_matches('-');
    if encoded.is_empty() {
        return None;
    }
    let mut best = (0, root.to_path_buf());
    // Names that encode to nothing can hide a large subtree, so they are
    // only entered when the ordinary walk finds nothing.
    for zero_width in [false, true] {
        let mut walk = Walk {
            total: encoded.len(),
            best,
            seen: HashMap::new(),
            budget: MAX_DECODE_VISITS,
            zero_width,
        };
        if let Some(found) = walk.descend(root, encoded, 0) {
            // Resolve symlinks (e.g. `/home/user` → `/data/home/user`) so the
            // result matches the canonicalized cwd filter.
            let found = std::fs::canonicalize(&found).unwrap_or(found);
            return Some(found.to_string_lossy().into_owned());
        }
        best = walk.best;
    }
    let (consumed, dir) = best;
    if consumed == 0 {
        return Some(
            root.join(encoded.replace('-', "/"))
                .to_string_lossy()
                .into_owned(),
        );
    }
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let rest = encoded[consumed..].trim_start_matches('-');
    Some(dir.join(rest).to_string_lossy().into_owned())
}

/// Guard against runaway recursion through components that consume no input
/// (names without ASCII alphanumerics). Ordinary components always consume
/// input, so their depth is bounded by the slug length.
const MAX_ZERO_WIDTH_DEPTH: usize = 256;
/// Upper bound on directories entered per decode.
const MAX_DECODE_VISITS: usize = 10_000;
/// Tokens probed per level when a directory cannot be listed (640 names).
const MAX_PROBE_TOKENS: usize = 4;

struct Walk {
    total: usize,
    /// Deepest existing directory reached: (bytes of input consumed, path).
    best: (usize, PathBuf),
    /// (directory identity, remaining length) states already explored, with
    /// the fewest zero-width components they were entered through.
    seen: HashMap<(DirId, usize), usize>,
    budget: usize,
    /// Whether names without ASCII alphanumerics may be entered.
    zero_width: bool,
}

#[cfg(unix)]
type DirId = (u64, u64);
#[cfg(not(unix))]
type DirId = PathBuf;

#[cfg(unix)]
fn dir_id(_path: &Path, meta: &std::fs::Metadata) -> Option<DirId> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn dir_id(path: &Path, _meta: &std::fs::Metadata) -> Option<DirId> {
    std::fs::canonicalize(path).ok()
}

fn list_dir(dir: &Path) -> DirListing {
    if let Ok(cache) = LISTING_CACHE.lock() {
        if let Some(listing) = cache.get(dir) {
            return listing.clone();
        }
    }
    let listing = std::fs::read_dir(dir)
        .ok()
        .map(|entries| Arc::new(entries.flatten().map(|e| e.file_name()).collect()));
    if let Ok(mut cache) = LISTING_CACHE.lock() {
        cache.insert(dir.to_path_buf(), listing.clone());
    }
    listing
}

/// Candidate names for a directory that can be searched but not listed
/// (mode `--x`): the next few tokens joined by every combination of the
/// separators Cursor folds into `-`, with and without a leading or trailing
/// separator (which the encoding trims). Only single `.`, `_` or `-`
/// separators are tried; runs such as `__tests__` or other symbols are not.
fn probe_names(rest: &str) -> Vec<OsString> {
    let tokens: Vec<&str> = rest.split('-').take(MAX_PROBE_TOKENS).collect();
    let mut names = Vec::new();
    let mut prefixes = vec![tokens[0].to_string()];
    for (i, token) in tokens.iter().enumerate() {
        if i > 0 {
            prefixes = prefixes
                .iter()
                .flat_map(|p| ["-", "_", "."].map(|sep| format!("{p}{sep}{token}")))
                .collect();
        }
        for name in &prefixes {
            for lead in ["", ".", "_", "-"] {
                for trail in ["", ".", "_", "-"] {
                    names.push(OsString::from(format!("{lead}{name}{trail}")));
                }
            }
        }
    }
    names
}

impl Walk {
    fn descend(&mut self, dir: &Path, rest: &str, zero_width_depth: usize) -> Option<PathBuf> {
        if zero_width_depth >= MAX_ZERO_WIDTH_DEPTH || self.budget == 0 {
            return None;
        }
        self.budget -= 1;
        let names = list_dir(dir).unwrap_or_else(|| Arc::new(probe_names(rest)));
        let mut candidates: Vec<(String, PathBuf, Option<DirId>)> = names
            .iter()
            .filter_map(|name| {
                let enc = encode_cursor_component(&name.to_string_lossy());
                // A name without ASCII alphanumerics (`日本語`, `_`) leaves no
                // trace in the encoding: it consumes nothing.
                let matches = (enc.is_empty() && self.zero_width)
                    || (!enc.is_empty()
                        && rest.starts_with(&enc)
                        && matches!(rest.as_bytes().get(enc.len()), None | Some(b'-')));
                if !matches {
                    return None;
                }
                // `metadata` follows symlinks, so `/home/user` → `/data/home/user`
                // counts. Continue from the resolved path so that chains of
                // symlinks never pile up (and hit the kernel's loop limit).
                let path = dir.join(name);
                let meta = std::fs::metadata(&path).ok().filter(|m| m.is_dir())?;
                let path = std::fs::canonicalize(&path).unwrap_or(path);
                let id = dir_id(&path, &meta);
                Some((enc, path, id))
            })
            .collect();
        // Longest match first (fewest components), zero-width names last;
        // ties broken by path for deterministic output.
        candidates.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.1.cmp(&b.1)));
        for (enc, path, id) in candidates {
            let remaining = rest[enc.len()..].trim_start_matches('-');
            if remaining.is_empty() && !enc.is_empty() {
                return Some(path);
            }
            let zero_width_depth = zero_width_depth + usize::from(enc.is_empty());
            // A state explored before has failed (success returns at once),
            // unless the zero-width depth limit cut that attempt shorter.
            if let Some(id) = id {
                let seen = self.seen.entry((id, remaining.len())).or_insert(usize::MAX);
                if *seen <= zero_width_depth {
                    continue;
                }
                *seen = zero_width_depth;
            }
            let consumed = self.total - remaining.len();
            if consumed > self.best.0 {
                self.best = (consumed, path.clone());
            }
            if let Some(found) = self.descend(&path, remaining, zero_width_depth) {
                return Some(found);
            }
        }
        None
    }
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
    fn prompts_in_session_json(&self) -> bool {
        true
    }
    fn prompts_per_jsonl_line(&self) -> bool {
        true
    }

    fn can_memory(&self) -> bool {
        true
    }

    /// User rules live in `$HOME/.cursor/rules` even when `CURSOR_DATA_DIR`
    /// moves the session data.
    fn global_memory_sources(&self, home: &Path) -> Vec<MemorySource> {
        vec![MemorySource::new(
            &home.join(".cursor/rules"),
            "**/*.mdc",
            MemoryKind::Rule,
        )]
    }

    fn project_memory_sources(&self, dir: &Path) -> Vec<MemorySource> {
        vec![
            MemorySource::new(dir, ".cursorrules", MemoryKind::Instruction),
            MemorySource::new(dir, ".cursor/rules/**/*.mdc", MemoryKind::Rule),
        ]
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

    fn subagent_glob_patterns(&self) -> &'static [&'static str] {
        &[".cursor/projects/*/agent-transcripts/*/subagents/*.jsonl"]
    }

    fn parent_session_id(&self, path: &Path) -> Option<String> {
        subagent_parent(path).map(str::to_string)
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.cursor/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        if let Some(mmap) = mmap_file(path) {
            self.iter_messages_from_bytes(path, &mmap, visit);
        }
    }

    fn iter_messages_from_bytes(
        &self,
        _path: &Path,
        data: &[u8],
        visit: &mut dyn FnMut(Message) -> bool,
    ) {
        for_each_jsonl_value_bytes(data, |val| {
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

    // Subagent transcripts are not resumable chats of their own.
    fn resolve_resume_id(&self, path: &Path, home: &Path) -> Option<String> {
        if subagent_parent(path).is_some() {
            return None;
        }
        self.session_id(path, home)
    }

    fn session_id(&self, path: &Path, _home: &Path) -> Option<String> {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .filter(|id| is_safe_cli_id(id))
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

    #[test]
    fn resume_id_rejects_option_like_stems() {
        let path = Path::new("/h/.cursor/projects/p/agent-transcripts/--yolo.jsonl");
        assert_eq!(PLUGIN.resolve_resume_id(path, Path::new("/h")), None);
    }

    #[test]
    fn decode_empty_is_none() {
        let (_tmp, root) = temp_root();
        assert_eq!(decode_cursor_path_inner(&root, ""), None);
        assert_eq!(decode_cursor_path_inner(&root, "---"), None);
    }

    #[test]
    fn decode_unknown_tree_reads_dashes_as_slashes() {
        let (_tmp, root) = temp_root();
        assert_eq!(
            decode(&root, "Users-u-src-ah"),
            root.join("Users/u/src/ah").to_string_lossy()
        );
    }

    #[test]
    fn decode_prefers_longest_component_on_ambiguous_leaf() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["p/foo/bar", "p/foo-bar"]);
        assert_eq!(
            decode(&root, "p-foo-bar"),
            root.join("p/foo-bar").to_string_lossy()
        );
    }

    #[test]
    fn decode_descends_through_dirs_without_ascii_alphanumerics() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["Users/u/日本語/深い/leaf", "a/_/b"]);
        assert_eq!(
            decode(&root, "Users-u-leaf"),
            root.join("Users/u/日本語/深い/leaf").to_string_lossy()
        );
        assert_eq!(decode(&root, "a-b"), root.join("a/_/b").to_string_lossy());
    }

    #[test]
    fn decode_prefers_paths_without_zero_width_names() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["p-foo/日本/bar", "p/foo/bar"]);
        assert_eq!(
            decode(&root, "p-foo-bar"),
            root.join("p/foo/bar").to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn decode_probes_through_unlistable_parent() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, root) = temp_root();
        let names = ["my_proj.v2", ".a-b", "_c", "-d", "e_"];
        for name in names {
            mkdirs(&root, &[format!("locked/{name}/.hidden").as_str()]);
        }
        let locked = root.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o111)).unwrap();
        let got: Vec<String> = names
            .iter()
            .map(|name| {
                decode(
                    &root,
                    &format!("locked-{}-hidden", encode_cursor_component(name)),
                )
            })
            .collect();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let want: Vec<String> = names
            .iter()
            .map(|name| {
                root.join(format!("locked/{name}/.hidden"))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn decode_handles_deep_paths() {
        let (_tmp, root) = temp_root();
        let rel = vec!["a"; 300].join("/");
        mkdirs(&root, &[rel.as_str()]);
        assert_eq!(
            decode(&root, &vec!["a"; 300].join("-")),
            root.join(&rel).to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn decode_does_not_re_explore_symlink_cycles() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["P"]);
        // `a` and `a_` both encode to `a` and lead back to `P`.
        std::os::unix::fs::symlink(root.join("P"), root.join("P/a")).unwrap();
        std::os::unix::fs::symlink(root.join("P"), root.join("P/a_")).unwrap();
        let started = std::time::Instant::now();
        let got = decode(&root, &format!("P-{}missing", "a-".repeat(24)));
        assert!(got.ends_with("missing"), "{got}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn decode_reenters_states_cut_short_by_the_depth_limit() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["p/x/日本/leaf"]);
        let mut chain = root.join("p-x");
        std::fs::create_dir(&chain).unwrap();
        for _ in 0..MAX_ZERO_WIDTH_DEPTH - 2 {
            chain.push("_");
            std::fs::create_dir(&chain).unwrap();
        }
        std::os::unix::fs::symlink(root.join("p/x"), chain.join("_")).unwrap();
        assert_eq!(
            decode(&root, "p-x-leaf"),
            root.join("p/x/日本/leaf").to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn decode_follows_long_symlink_chains() {
        let (_tmp, root) = temp_root();
        mkdirs(&root, &["P/leaf"]);
        // `a_` points back to `P`; `a` points to `a_`. Following either many
        // times would exceed the kernel's symlink limit on an unresolved path.
        std::os::unix::fs::symlink(".", root.join("P/a_")).unwrap();
        std::os::unix::fs::symlink("a_", root.join("P/a")).unwrap();
        assert_eq!(
            decode(&root, &format!("P-{}leaf", "a-".repeat(45))),
            root.join("P/leaf").to_string_lossy()
        );
    }
}

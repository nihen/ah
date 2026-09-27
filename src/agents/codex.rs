use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use regex::Regex;

use super::AgentPlugin;
use super::Message;
use super::common::for_each_jsonl_value;
use super::common::format_mtime;
use super::common::read_first_line_json;
use super::common::strip_home;

static RE_CODEX_SESSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".*/sessions/(\d+/\d+/\d+)/.*").unwrap());
static RE_CODEX_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"rollout-(\d{4}-\d{2}-\d{2})T(\d{2})-(\d{2})").unwrap());
static RE_CODEX_ROLLOUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^rollout-[\dT-]+-(.+)$").unwrap());

/// Codex home of a session file: the parent of the `sessions` or
/// `archived_sessions` directory that contains it.
fn colocated_codex_home(path: &Path) -> Option<&Path> {
    path.ancestors()
        .find(|dir| {
            matches!(
                dir.file_name().and_then(|s| s.to_str()),
                Some("sessions" | "archived_sessions")
            )
        })
        .and_then(|dir| dir.parent())
}

type TitleIndex = HashMap<String, String>;

/// Size and mtime of an index file; `None` when it does not exist.
type IndexStamp = Option<(u64, Option<SystemTime>)>;

/// Parsed `session_index.jsonl` files, keyed by path. Every listed session
/// needs a title, so an index is re-read only when its size or mtime
/// changes (e.g. a rename while `ah log -i` waits for a selection).
type TitleIndexCache = HashMap<PathBuf, (IndexStamp, Arc<TitleIndex>)>;

static TITLE_INDEXES: LazyLock<Mutex<TitleIndexCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn title_index(index_path: &Path) -> Arc<TitleIndex> {
    let stamp = fs::metadata(index_path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    let mut cache = TITLE_INDEXES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached_stamp, index)) = cache.get(index_path) {
        if *cached_stamp == stamp {
            return index.clone();
        }
    }
    let index = Arc::new(read_title_index(index_path));
    cache.insert(index_path.to_path_buf(), (stamp, index.clone()));
    index
}

/// Map each session id to its latest `thread_name`. Codex appends a new
/// line on every rename, so later lines win.
fn read_title_index(index_path: &Path) -> TitleIndex {
    let mut names = TitleIndex::new();
    let Ok(index_file) = fs::File::open(index_path) else {
        return names;
    };
    for line in BufReader::new(index_file).lines() {
        let Ok(line) = line else { break };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = val.get("id").and_then(|v| v.as_str());
        let name = val.get("thread_name").and_then(|v| v.as_str());
        if let (Some(id), Some(name)) = (id, name) {
            if !name.is_empty() {
                names.insert(id.to_string(), name.to_string());
            }
        }
    }
    names
}

/// Latest thread name of `session_id`: from the index next to the session
/// file, then from the configured Codex base (`CODEX_HOME` or `~/.codex`).
fn latest_thread_name(path: &Path, session_id: &str) -> Option<String> {
    latest_thread_name_in(path, session_id, crate::config::resolve_agent_base("codex"))
}

fn latest_thread_name_in(
    path: &Path,
    session_id: &str,
    configured: Option<PathBuf>,
) -> Option<String> {
    let colocated = colocated_codex_home(path).map(Path::to_path_buf);
    let mut homes = colocated.into_iter().chain(configured).collect::<Vec<_>>();
    homes.dedup();
    homes.iter().find_map(|home| {
        title_index(&home.join("session_index.jsonl"))
            .get(session_id)
            .cloned()
    })
}

pub static PLUGIN: CodexPlugin = CodexPlugin;

pub struct CodexPlugin;

impl AgentPlugin for CodexPlugin {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn description(&self) -> &'static str {
        "Codex CLI (OpenAI)"
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative path of cwd)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[
            ".codex/sessions/**/*.jsonl",
            ".codex/archived_sessions/**/*.jsonl",
        ]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        &["/.codex/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        for_each_jsonl_value(path, |val| {
            if val.get("type").and_then(|v| v.as_str()) != Some("response_item") {
                return true;
            }

            match val.pointer("/payload/role").and_then(|v| v.as_str()) {
                Some("user") => {
                    if let Some(contents) =
                        val.pointer("/payload/content").and_then(|v| v.as_array())
                    {
                        for item in contents {
                            let is_user_text = matches!(
                                item.get("type").and_then(|v| v.as_str()),
                                Some("input_text" | "text")
                            );
                            if is_user_text {
                                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                    if !text.starts_with('<')
                                        && !text.starts_with("# ")
                                        && !visit(Message::user(text.to_string()))
                                    {
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                }
                Some("assistant") => {
                    if let Some(contents) =
                        val.pointer("/payload/content").and_then(|v| v.as_array())
                    {
                        for item in contents {
                            if item.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                    if !visit(Message::assistant(text.to_string())) {
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            true
        });
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        if let Some(cwd) = self.resolve_cwd(path, home) {
            Some(strip_home(&cwd, home))
        } else {
            RE_CODEX_SESSIONS
                .captures(&path.to_string_lossy())
                .map(|caps| caps[1].to_string())
                .or_else(|| Some("?".to_string()))
        }
    }

    fn resolve_date(&self, path: &Path, mtime: SystemTime) -> Option<String> {
        RE_CODEX_DATE
            .captures(&path.to_string_lossy())
            .map(|caps| format!("{} {}:{}", &caps[1], &caps[2], &caps[3]))
            .or_else(|| Some(format_mtime(mtime)))
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        let val = read_first_line_json(path)?;
        val.pointer("/payload/cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    fn resolve_title(&self, path: &Path, _home: &Path) -> Option<String> {
        let val = read_first_line_json(path)?;
        let session_id = val.pointer("/payload/id")?.as_str()?;
        latest_thread_name(path, session_id)
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        if path.to_string_lossy().contains("/archived_sessions/") {
            return None;
        }

        let val = read_first_line_json(path)?;
        if let Some(id) = val.pointer("/payload/id").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }

        let stem = path.file_stem()?.to_string_lossy();
        RE_CODEX_ROLLOUT
            .captures(&stem)
            .map(|caps| caps[1].to_string())
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["codex".to_string(), "resume".to_string(), id])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "019c1dbb-cd96-70d3-baba-ef490967626c";

    fn write_session(codex_home: &Path, subdir: &str) -> PathBuf {
        let dir = codex_home.join(subdir).join("2026/09/27");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-09-27T10-00-00-{ID}.jsonl"));
        let meta =
            format!(r#"{{"type":"session_meta","payload":{{"id":"{ID}","cwd":"/tmp/proj"}}}}"#);
        fs::write(&path, format!("{meta}\n")).unwrap();
        path
    }

    fn write_index(codex_home: &Path, lines: &[(&str, &str)]) {
        let body: String = lines
            .iter()
            .map(|(id, name)| format!(r#"{{"id":"{id}","thread_name":"{name}"}}"#) + "\n")
            .collect();
        fs::write(codex_home.join("session_index.jsonl"), body).unwrap();
    }

    #[test]
    fn title_uses_latest_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let session = write_session(tmp.path(), "sessions");
        write_index(
            tmp.path(),
            &[
                (ID, "first name"),
                ("other-id", "unrelated"),
                (ID, "renamed"),
                ("other-id", "unrelated again"),
            ],
        );
        assert_eq!(
            PLUGIN.resolve_title(&session, Path::new("/nonexistent")),
            Some("renamed".to_string())
        );
    }

    #[test]
    fn title_reads_index_next_to_custom_codex_home() {
        // A CODEX_HOME-style directory that is not `~/.codex`.
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("my-codex");
        let session = write_session(&codex_home, "archived_sessions");
        write_index(&codex_home, &[(ID, "archived title")]);
        assert_eq!(
            PLUGIN.resolve_title(&session, tmp.path()),
            Some("archived title".to_string())
        );
    }

    #[test]
    fn title_falls_back_when_colocated_index_is_missing() {
        // e.g. an `extra_patterns` archive of session files without an index
        let tmp = tempfile::tempdir().unwrap();
        let session = write_session(&tmp.path().join("archive"), "sessions");
        let configured = tmp.path().join("codex-home");
        fs::create_dir_all(&configured).unwrap();
        write_index(&configured, &[(ID, "from configured base")]);
        assert_eq!(
            latest_thread_name_in(&session, ID, Some(configured)),
            Some("from configured base".to_string())
        );
    }

    #[test]
    fn title_reflects_a_rename_after_the_index_was_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let session = write_session(tmp.path(), "sessions");
        write_index(tmp.path(), &[(ID, "before")]);
        let home = Path::new("/nonexistent");
        assert_eq!(
            PLUGIN.resolve_title(&session, home),
            Some("before".to_string())
        );
        write_index(tmp.path(), &[(ID, "before"), (ID, "after rename")]);
        assert_eq!(
            PLUGIN.resolve_title(&session, home),
            Some("after rename".to_string())
        );
    }

    #[test]
    fn title_ignores_lines_that_only_mention_the_id() {
        let tmp = tempfile::tempdir().unwrap();
        let session = write_session(tmp.path(), "sessions");
        write_index(tmp.path(), &[(ID, "real"), ("other-id", ID)]);
        assert_eq!(
            PLUGIN.resolve_title(&session, Path::new("/nonexistent")),
            Some("real".to_string())
        );
    }
}

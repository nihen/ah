use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use super::AgentPlugin;
use super::Message;
use super::common::canonicalize_if_exists;
use super::common::for_each_jsonl_value;
use super::common::is_safe_cli_id;
use super::common::percent_decode;
use super::common::strip_home;
use super::common::tagged_user_body;

pub static PLUGIN: AgyPlugin = AgyPlugin;

/// Google Antigravity CLI (`agy`).
///
/// Layout: `~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/transcript.jsonl`
/// The transcript itself carries no cwd; it is resolved through the cache files
/// under `~/.gemini/antigravity-cli/cache/` (`last_conversations.json`,
/// `conversation_metadata.json`).
pub struct AgyPlugin;

/// conversation-id → cwd index built from the Antigravity cache files.
struct CwdIndex {
    by_id: HashMap<String, String>,
}

impl CwdIndex {
    fn load(base: &Path) -> Self {
        let mut by_id = HashMap::new();

        // cache/conversation_metadata.json: {"conversations": {"<id>": {"summary": {"WorkspaceURIs": ["file:///path", ...]}}}}
        if let Ok(content) = fs::read_to_string(base.join("cache/conversation_metadata.json")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(convs) = val.get("conversations").and_then(|v| v.as_object()) {
                    for (id, entry) in convs {
                        let uris = entry
                            .pointer("/summary/WorkspaceURIs")
                            .and_then(|v| v.as_array());
                        let Some(uris) = uris else { continue };
                        let paths: Vec<String> = uris
                            .iter()
                            .filter_map(|u| u.as_str())
                            .filter_map(|u| u.strip_prefix("file://"))
                            .map(percent_decode)
                            .filter(|p| !p.is_empty())
                            .collect();
                        // `agy --add-dir /tmp` appends /tmp as an extra workspace;
                        // prefer the real project dir, fall back to whatever is first.
                        let cwd = paths
                            .iter()
                            .find(|p| p.as_str() != "/tmp")
                            .or_else(|| paths.first())
                            .cloned();
                        if let Some(cwd) = cwd {
                            by_id.insert(id.clone(), canonicalize_if_exists(&cwd));
                        }
                    }
                }
            }
        }

        // cache/last_conversations.json: {"<cwd>": "<id>"} — newer, overrides metadata.
        if let Ok(content) = fs::read_to_string(base.join("cache/last_conversations.json")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(map) = val.as_object() {
                    for (cwd, id) in map {
                        if let Some(id) = id.as_str() {
                            by_id.insert(id.to_string(), canonicalize_if_exists(cwd));
                        }
                    }
                }
            }
        }

        Self { by_id }
    }
}

static CWD_INDEX_CACHE: LazyLock<Mutex<HashMap<PathBuf, Arc<CwdIndex>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cwd_index(base: &Path) -> Arc<CwdIndex> {
    if let Ok(cache) = CWD_INDEX_CACHE.lock() {
        if let Some(idx) = cache.get(base) {
            return Arc::clone(idx);
        }
    }
    let idx = Arc::new(CwdIndex::load(base));
    if let Ok(mut cache) = CWD_INDEX_CACHE.lock() {
        cache.insert(base.to_path_buf(), Arc::clone(&idx));
    }
    idx
}

impl AgyPlugin {
    /// Walk up from the transcript to find `brain/<id>`; returns (base, id).
    /// base is the directory containing `brain/` (i.e. `.../antigravity-cli`).
    fn split_path(path: &Path) -> Option<(PathBuf, String)> {
        let mut current = path.parent();
        while let Some(dir) = current {
            if let Some(parent) = dir.parent() {
                if parent.file_name().and_then(|s| s.to_str()) == Some("brain") {
                    let id = dir.file_name()?.to_string_lossy().to_string();
                    return Some((parent.parent()?.to_path_buf(), id));
                }
            }
            current = dir.parent();
        }
        None
    }

    fn message_from_value(val: &serde_json::Value) -> Option<Message> {
        let source = val.get("source").and_then(|v| v.as_str())?;
        let kind = val.get("type").and_then(|v| v.as_str())?;
        match (source, kind) {
            ("USER_EXPLICIT", "USER_INPUT") => {
                let raw = val.get("content").and_then(|v| v.as_str())?;
                let text = tagged_user_body(raw, "USER_REQUEST")?;
                Some(Message::user(text.to_string()))
            }
            ("MODEL", "PLANNER_RESPONSE") => {
                let text = val.get("content").and_then(|v| v.as_str())?;
                if text.trim().is_empty() {
                    return None;
                }
                Some(Message::assistant(text.to_string()))
            }
            _ => None,
        }
    }
}

impl AgentPlugin for AgyPlugin {
    fn id(&self) -> &'static str {
        "agy"
    }

    fn description(&self) -> &'static str {
        "Antigravity CLI (Google)"
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn can_follow(&self) -> bool {
        true
    }

    fn project_desc(&self) -> &'static str {
        "basename of cwd (raw: home-relative cwd looked up by conversation id in antigravity-cli/cache/*.json)"
    }

    fn glob_patterns(&self) -> &'static [&'static str] {
        &[".gemini/antigravity-cli/brain/*/.system_generated/logs/transcript.jsonl"]
    }

    fn path_markers(&self) -> &'static [&'static str] {
        // Longer than gemini's "/.gemini/" so it wins the longest-marker match.
        &["/.gemini/antigravity-cli/"]
    }

    fn iter_messages(&self, path: &Path, visit: &mut dyn FnMut(Message) -> bool) {
        for_each_jsonl_value(path, |val| match Self::message_from_value(val) {
            Some(msg) => visit(msg),
            None => true,
        });
    }

    fn messages_from_value(&self, val: &serde_json::Value) -> Vec<Message> {
        Self::message_from_value(val).into_iter().collect()
    }

    fn resolve_cwd(&self, path: &Path, _home: &Path) -> Option<String> {
        let (base, id) = Self::split_path(path)?;
        cwd_index(&base).by_id.get(&id).cloned()
    }

    fn resolve_project(&self, path: &Path, home: &Path) -> Option<String> {
        // Raw identifier: home-relative cwd (like codex). Basename is applied
        // by the resolver. Unknown cwd → "?" (same convention as gemini).
        self.resolve_cwd(path, home)
            .map(|cwd| strip_home(&cwd, home))
            .or_else(|| Some("?".to_string()))
    }

    fn resolve_resume_id(&self, path: &Path, _home: &Path) -> Option<String> {
        Self::split_path(path)
            .map(|(_, id)| id)
            .filter(|id| is_safe_cli_id(id))
    }

    fn resume_args(&self, path: &Path, home: &Path) -> Option<Vec<String>> {
        let id = self.resolve_resume_id(path, home)?;
        Some(vec!["agy".to_string(), "--conversation".to_string(), id])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_transcript(base: &Path, id: &str, body: &str) -> PathBuf {
        let dir = base.join("brain").join(id).join(".system_generated/logs");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("transcript.jsonl");
        fs::File::create(&p)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        p
    }

    #[test]
    fn parses_user_request_and_planner_response() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(".gemini/antigravity-cli");
        let body = concat!(
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-08-20T08:11:44Z","content":"<USER_REQUEST>\nfix the bug\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: x\n</ADDITIONAL_METADATA>"}"#,
            "\n",
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"view_file","args":{}}]}"#,
            "\n",
            r#"{"step_index":2,"source":"MODEL","type":"VIEW_FILE","status":"DONE","content":"file body"}"#,
            "\n",
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Done.","thinking":"..."}"#,
            "\n",
            r#"{"step_index":4,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"sys"}"#,
            "\n",
        );
        let path = write_transcript(&base, "11111111-aaaa", body);
        let mut msgs = Vec::new();
        PLUGIN.iter_messages(&path, &mut |m| {
            msgs.push(m);
            true
        });
        assert_eq!(
            msgs,
            vec![
                Message::user("fix the bug".to_string()),
                Message::assistant("Done.".to_string()),
            ]
        );
    }

    #[test]
    fn resolves_cwd_from_cache_files_and_resume_id_from_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(".gemini/antigravity-cli");
        fs::create_dir_all(base.join("cache")).unwrap();
        fs::write(
            base.join("cache/conversation_metadata.json"),
            r#"{"conversations":{
                "aaaa":{"summary":{"WorkspaceURIs":["file:///tmp","file:///data/home/me/old%20proj"]}},
                "bbbb":{"summary":{"WorkspaceURIs":["file:///data/home/me/stale"]}}
            }}"#,
        )
        .unwrap();
        fs::write(
            base.join("cache/last_conversations.json"),
            r#"{"/data/home/me/new-proj":"bbbb"}"#,
        )
        .unwrap();
        let home = tmp.path();

        let a = write_transcript(&base, "aaaa", "");
        let b = write_transcript(&base, "bbbb", "");
        let c = write_transcript(&base, "cccc", "");

        assert_eq!(
            PLUGIN.resolve_cwd(&a, home).as_deref(),
            Some("/data/home/me/old proj")
        );
        assert_eq!(
            PLUGIN.resolve_project(&a, home).as_deref(),
            Some("/data/home/me/old proj")
        );
        // last_conversations.json wins over conversation_metadata.json
        assert_eq!(
            PLUGIN.resolve_cwd(&b, home).as_deref(),
            Some("/data/home/me/new-proj")
        );
        assert_eq!(PLUGIN.resolve_cwd(&c, home), None);

        // cwd under home → home-relative raw project
        let d = write_transcript(&base, "dddd", "");
        fs::write(
            base.join("cache/last_conversations.json"),
            format!(
                r#"{{"/data/home/me/new-proj":"bbbb","{}":"dddd"}}"#,
                home.join("src/proj").to_string_lossy()
            ),
        )
        .unwrap();
        // index is cached per base dir; use a fresh base to pick up the new file
        let base2 = tmp.path().join("alt/.gemini/antigravity-cli");
        fs::create_dir_all(base2.join("cache")).unwrap();
        fs::copy(
            base.join("cache/last_conversations.json"),
            base2.join("cache/last_conversations.json"),
        )
        .unwrap();
        let _ = d;
        let d2 = write_transcript(&base2, "dddd", "");
        assert_eq!(
            PLUGIN.resolve_project(&d2, home).as_deref(),
            Some("src/proj")
        );
        assert_eq!(PLUGIN.resolve_project(&c, home).as_deref(), Some("?"));

        assert_eq!(PLUGIN.resolve_resume_id(&c, home).as_deref(), Some("cccc"));
        assert_eq!(
            PLUGIN.resume_args(&c, home),
            Some(vec![
                "agy".to_string(),
                "--conversation".to_string(),
                "cccc".to_string()
            ])
        );
    }
}

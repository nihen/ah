use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::agents::{self, AgentPlugin};

static AGENT_REGISTRY: OnceLock<Vec<AgentDef>> = OnceLock::new();
static REMOTE_REGISTRY: OnceLock<Vec<RemoteDef>> = OnceLock::new();

/// Runtime agent definition (built-in + config merged).
pub struct AgentDef {
    pub id: String,
    pub plugin: &'static dyn AgentPlugin,
    pub glob_patterns: Vec<String>,
    pub path_markers: Vec<String>,
    pub disabled: bool,
    pub description: String,
    pub is_builtin: bool,
}

impl AgentDef {
    pub fn matches_path(&self, path: &Path) -> bool {
        let s = path.to_string_lossy();
        self.path_markers.iter().any(|marker| s.contains(marker))
    }
}

/// Remote host definition for SSH session aggregation.
pub struct RemoteDef {
    pub name: String,
    pub host: String,
    pub ah_path: String,
}

/// TOML deserialization structures.
#[derive(Deserialize, Default)]
struct AhrcConfig {
    #[serde(default)]
    agents: HashMap<String, AgentEntry>,
    #[serde(default)]
    remotes: HashMap<String, RemoteEntry>,
}

#[derive(Deserialize, Default)]
struct AgentEntry {
    plugin: Option<String>,
    file_patterns: Option<Vec<String>>,
    extra_patterns: Option<Vec<String>>,
    disabled: Option<bool>,
}

#[derive(Deserialize, Default)]
struct RemoteEntry {
    /// Optional in TOML so that one malformed `[remotes.*]` entry does not
    /// invalidate the entire `~/.ahrc` parse. Required at the validation
    /// step in `load_remotes`.
    host: Option<String>,
    ah_path: Option<String>,
}

/// Built-in agent definition (env var + default directory prefix).
struct BuiltinInfo {
    env_var: &'static str,
    default_prefix: &'static str,
}

fn builtin_env_info(agent_id: &str) -> Option<BuiltinInfo> {
    match agent_id {
        "claude" => Some(BuiltinInfo {
            env_var: "CLAUDE_CONFIG_DIR",
            default_prefix: ".claude",
        }),
        "codex" => Some(BuiltinInfo {
            env_var: "CODEX_HOME",
            default_prefix: ".codex",
        }),
        "gemini" => Some(BuiltinInfo {
            env_var: "GEMINI_CLI_HOME",
            default_prefix: ".gemini",
        }),
        "copilot" => Some(BuiltinInfo {
            env_var: "COPILOT_HOME",
            default_prefix: ".copilot",
        }),
        "cursor" => Some(BuiltinInfo {
            env_var: "CURSOR_CONFIG_DIR",
            default_prefix: ".cursor",
        }),
        "grok" => Some(BuiltinInfo {
            env_var: "GROK_HOME",
            default_prefix: ".grok",
        }),
        // opencode follows the XDG base directory spec: $XDG_DATA_HOME/opencode.
        "opencode" => Some(BuiltinInfo {
            env_var: "XDG_DATA_HOME",
            default_prefix: ".local/share",
        }),
        // "agy": Antigravity CLI has no documented env var for its data dir
        // (~/.gemini/antigravity-cli); patterns are always home-relative.
        _ => None,
    }
}

/// Expand a leading `~/` to the home directory path.
fn expand_tilde(s: &str, home: &Path) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest).to_string_lossy().to_string()
    } else {
        s.to_string()
    }
}

/// Expand `~` to home directory. Validate that patterns start with `~/` or `/`.
fn expand_pattern(pattern: &str, home: &Path) -> Result<String, String> {
    if pattern.starts_with("~/") || pattern.starts_with('/') {
        Ok(expand_tilde(pattern, home))
    } else {
        Err(format!(
            "Invalid pattern '{}': must start with ~/ or /",
            pattern
        ))
    }
}

/// Derive path_markers from glob patterns.
/// Uses the fixed (non-glob) prefix of each pattern as the marker.
fn derive_path_markers(patterns: &[String]) -> Vec<String> {
    let mut markers = Vec::new();
    for pattern in patterns {
        // Take the path up to the first glob character
        let prefix: String = pattern
            .chars()
            .take_while(|c| !matches!(c, '*' | '?' | '['))
            .collect();
        // Trim trailing slash
        let prefix = prefix.trim_end_matches('/');
        if !prefix.is_empty() && !markers.contains(&prefix.to_string()) {
            markers.push(prefix.to_string());
        }
    }
    markers
}

/// Path marker for an `extra_patterns` entry of a built-in agent: the
/// literal part of the pattern. An exact path is its own marker, a glob
/// within the file name keeps the literal prefix (`~/archive/claude-*`),
/// and a glob spanning directories is cut back to a directory boundary
/// (`~/.c*/x.db` -> `~`). Markers are matched as substrings and the longest
/// wins, so a marker that also matches HOME itself (or an alias of it), or
/// part of another agent's default location (`~/.c*.db` -> `~/.c` inside
/// `~/.claude/`), would claim other agents' files and is not returned.
fn extra_path_marker(pattern: &str, home: &Path, other_patterns: &[String]) -> Option<String> {
    let fixed: String = pattern
        .chars()
        .take_while(|c| !matches!(c, '*' | '?' | '['))
        .collect();
    let marker = if pattern[fixed.len()..].contains('/') {
        &fixed[..fixed.rfind('/')?]
    } else {
        fixed.as_str()
    };
    let marker = marker.trim_end_matches('/');
    if marker.is_empty()
        || home.to_string_lossy().contains(marker)
        || other_patterns.iter().any(|p| p.contains(marker))
    {
        return None;
    }
    Some(marker.to_string())
}

/// Resolve the base directory for a built-in agent (respects env var override).
pub fn resolve_agent_base(agent_id: &str) -> Option<std::path::PathBuf> {
    let info = builtin_env_info(agent_id)?;
    if let Ok(custom_dir) = std::env::var(info.env_var) {
        let trimmed = custom_dir.trim();
        if !trimmed.is_empty() {
            let home = crate::agents::common::canonical_home();
            return Some(std::path::PathBuf::from(expand_tilde(trimmed, &home)));
        }
    }
    let home = crate::agents::common::canonical_home();
    Some(home.join(info.default_prefix))
}

/// Apply env var override to built-in patterns.
/// If the env var is set, replace the default prefix with the env var value.
fn apply_env_override(patterns: &[&str], home: &Path, info: &BuiltinInfo) -> Vec<String> {
    if let Ok(custom_dir) = std::env::var(info.env_var) {
        let trimmed = custom_dir.trim();
        if trimmed.is_empty() {
            return patterns
                .iter()
                .map(|p| home.join(p).to_string_lossy().to_string())
                .collect();
        }
        let custom_dir = expand_tilde(trimmed, home);
        let custom_dir = custom_dir.trim_end_matches('/');
        patterns
            .iter()
            .map(|p| {
                // Replace the default prefix (e.g., .claude) with custom dir
                if let Some(rest) = p.strip_prefix(info.default_prefix) {
                    format!("{}{}", custom_dir, rest)
                } else {
                    home.join(p).to_string_lossy().to_string()
                }
            })
            .collect()
    } else {
        patterns
            .iter()
            .map(|p| home.join(p).to_string_lossy().to_string())
            .collect()
    }
}

/// Load config and build agent + remote registries.
fn load_config(home: &Path) -> (Vec<AgentDef>, Vec<RemoteDef>) {
    // 1. Build defaults from built-in plugins
    let mut agents: Vec<AgentDef> = agents::all_plugins()
        .iter()
        .map(|plugin| {
            let id = plugin.id().to_string();
            let (glob_patterns, env_overridden) = if let Some(info) = builtin_env_info(&id) {
                let patterns = apply_env_override(plugin.glob_patterns(), home, &info);
                let overridden = std::env::var(info.env_var)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false);
                (patterns, overridden)
            } else {
                let patterns = plugin
                    .glob_patterns()
                    .iter()
                    .map(|p| home.join(p).to_string_lossy().to_string())
                    .collect();
                (patterns, false)
            };
            let path_markers = if env_overridden {
                derive_path_markers(&glob_patterns)
            } else {
                plugin
                    .path_markers()
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            };
            AgentDef {
                id,
                plugin: *plugin,
                glob_patterns,
                path_markers,
                disabled: false,
                description: plugin.description().to_string(),
                is_builtin: true,
            }
        })
        .collect();

    // 2. Read ~/.ahrc if it exists
    let ahrc_path = home.join(".ahrc");
    let config = if ahrc_path.exists() {
        match std::fs::read_to_string(&ahrc_path) {
            Ok(content) => match toml::from_str::<AhrcConfig>(&content) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("Warning: failed to parse ~/.ahrc: {}", e);
                    AhrcConfig::default()
                }
            },
            Err(e) => {
                eprintln!("Warning: failed to read ~/.ahrc: {}", e);
                AhrcConfig::default()
            }
        }
    } else {
        return (agents, Vec::new());
    };

    // Built-in locations, for rejecting extra-pattern markers that overlap them.
    let builtin_patterns: Vec<(String, Vec<String>)> = agents
        .iter()
        .map(|a| (a.id.clone(), a.glob_patterns.clone()))
        .collect();

    // 3. Apply config overrides
    for (agent_id, entry) in &config.agents {
        if let Some(existing) = agents.iter_mut().find(|a| a.id == *agent_id) {
            // Override built-in agent
            if entry.disabled.unwrap_or(false) {
                existing.disabled = true;
            }
            if let Some(extra) = &entry.extra_patterns {
                for pattern in extra {
                    match expand_pattern(pattern, home) {
                        Ok(expanded) => {
                            // Paths collected from extra locations must still
                            // map back to this agent.
                            let other_patterns: Vec<String> = builtin_patterns
                                .iter()
                                .filter(|(id, _)| id != agent_id)
                                .flat_map(|(_, patterns)| patterns.iter().cloned())
                                .collect();
                            match extra_path_marker(&expanded, home, &other_patterns) {
                                Some(marker) => {
                                    if !existing.path_markers.contains(&marker) {
                                        existing.path_markers.push(marker);
                                    }
                                }
                                None => eprintln!(
                                    "Warning: ~/.ahrc [agents.{}]: extra pattern '{}' cannot be told \
                                     apart from HOME or another agent's directory; ignored (use a \
                                     dedicated directory or name prefix)",
                                    agent_id, pattern
                                ),
                            }
                            existing.glob_patterns.push(expanded);
                        }
                        Err(e) => eprintln!("Warning: ~/.ahrc [agents.{}]: {}", agent_id, e),
                    }
                }
            }
        } else {
            // New custom agent
            let Some(plugin_name) = &entry.plugin else {
                eprintln!(
                    "Warning: ~/.ahrc [agents.{}]: 'plugin' is required for custom agents",
                    agent_id
                );
                continue;
            };
            let Some(plugin) = agents::find_builtin_plugin(plugin_name) else {
                eprintln!(
                    "Warning: ~/.ahrc [agents.{}]: unknown plugin '{}'",
                    agent_id, plugin_name
                );
                continue;
            };
            let Some(file_patterns) = &entry.file_patterns else {
                eprintln!(
                    "Warning: ~/.ahrc [agents.{}]: 'file_patterns' is required for custom agents",
                    agent_id
                );
                continue;
            };

            let mut glob_patterns = Vec::new();
            for pattern in file_patterns {
                match expand_pattern(pattern, home) {
                    Ok(expanded) => glob_patterns.push(expanded),
                    Err(e) => eprintln!("Warning: ~/.ahrc [agents.{}]: {}", agent_id, e),
                }
            }

            let path_markers = derive_path_markers(&glob_patterns);
            agents.push(AgentDef {
                id: agent_id.clone(),
                plugin,
                glob_patterns,
                path_markers,
                disabled: entry.disabled.unwrap_or(false),
                description: plugin.description().to_string(),
                is_builtin: false,
            });
        }
    }

    let remotes = load_remotes(&config);
    (agents, remotes)
}

/// Load remote definitions from parsed config.
fn load_remotes(config: &AhrcConfig) -> Vec<RemoteDef> {
    let mut remotes = Vec::new();
    for (name, entry) in &config.remotes {
        let host = match entry.host.as_deref().map(str::trim) {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => {
                eprintln!(
                    "Warning: remote '{}' is missing required `host`. Skipping.",
                    name
                );
                continue;
            }
        };
        let ah_path = entry
            .ah_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("ah")
            .to_string();
        if ah_path.starts_with('~') || (ah_path.contains('/') && !ah_path.starts_with('/')) {
            eprintln!(
                "Warning: remote '{}' has ah_path '{}' which is not an absolute path or bare command. \
                 Use an absolute path (e.g. /usr/local/bin/ah) or a bare command name (e.g. ah). Skipping.",
                name, ah_path
            );
            continue;
        }
        remotes.push(RemoteDef {
            name: name.clone(),
            host,
            ah_path,
        });
    }
    remotes.sort_by(|a, b| a.name.cmp(&b.name));
    remotes
}

/// Initialize the global agent and remote registries. Call once at startup.
pub fn init(home: &Path) {
    if AGENT_REGISTRY.get().is_some() && REMOTE_REGISTRY.get().is_some() {
        return;
    }
    let (agents, remotes) = load_config(home);
    AGENT_REGISTRY.get_or_init(|| agents);
    REMOTE_REGISTRY.get_or_init(|| remotes);
}

/// Get all agents (including disabled).
pub fn agents() -> &'static [AgentDef] {
    AGENT_REGISTRY
        .get()
        .expect("config::init() must be called before config::agents()")
}

/// Get all configured remotes.
pub fn remotes() -> &'static [RemoteDef] {
    REMOTE_REGISTRY
        .get()
        .expect("config::init() must be called before config::remotes()")
}

/// Find a remote by name.
pub fn find_remote(name: &str) -> Option<&'static RemoteDef> {
    remotes().iter().find(|r| r.name == name)
}

/// Get only active (non-disabled) agents.
pub fn active_agents() -> impl Iterator<Item = &'static AgentDef> {
    agents().iter().filter(|a| !a.disabled)
}

/// Find the best matching active agent for a path.
/// Prefers the agent with the longest matching path_marker (most specific match).
/// The longest match is chosen across *all* agents (including disabled ones) so
/// that disabling a specific agent (e.g. `agy` under `.gemini/antigravity-cli/`)
/// does not make its files fall through to a less specific agent (`gemini`).
pub fn find_agent_for_path(path: &Path) -> Option<&'static AgentDef> {
    best_agent_for_path(agents(), path)
}

fn best_agent_for_path<'a>(candidates: &'a [AgentDef], path: &Path) -> Option<&'a AgentDef> {
    let path_str = path.to_string_lossy();
    candidates
        .iter()
        .filter(|a| a.matches_path(path))
        .max_by_key(|a| {
            let longest = a
                .path_markers
                .iter()
                .filter(|m| path_str.contains(m.as_str()))
                .map(|m| m.len())
                .max()
                .unwrap_or(0);
            // On equal marker length prefer an active agent over a disabled one
            // (deterministic regardless of registry order).
            (longest, !a.disabled)
        })
        .filter(|a| !a.disabled)
}

/// Find an active agent's plugin by path.
pub fn find_plugin_for_path(path: &Path) -> &'static dyn AgentPlugin {
    find_agent_for_path(path)
        .map(|a| a.plugin)
        .unwrap_or_else(agents::unknown_plugin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_home() -> PathBuf {
        PathBuf::from("/Users/test")
    }

    #[test]
    fn test_expand_pattern_home() {
        let home = test_home();
        assert_eq!(
            expand_pattern("~/.claude/projects/*/*.jsonl", &home).unwrap(),
            "/Users/test/.claude/projects/*/*.jsonl"
        );
    }

    #[test]
    fn test_expand_pattern_absolute() {
        let home = test_home();
        assert_eq!(
            expand_pattern("/custom/path/*.jsonl", &home).unwrap(),
            "/custom/path/*.jsonl"
        );
    }

    #[test]
    fn test_expand_pattern_relative_error() {
        let home = test_home();
        assert!(expand_pattern("relative/path", &home).is_err());
    }

    #[test]
    fn test_derive_path_markers() {
        let patterns = vec!["/Users/test/.myagent/sessions/**/*.jsonl".to_string()];
        let markers = derive_path_markers(&patterns);
        assert_eq!(markers, vec!["/Users/test/.myagent/sessions"]);
    }

    #[test]
    fn test_derive_path_markers_fixed_prefix() {
        let patterns = vec!["/custom/path/sessions/*.jsonl".to_string()];
        let markers = derive_path_markers(&patterns);
        assert_eq!(markers, vec!["/custom/path/sessions"]);
    }

    #[test]
    fn test_derive_path_markers_longer_wins() {
        // mydev path has a longer fixed prefix than local
        let local = vec!["/Users/test/.claude/projects/*/*.jsonl".to_string()];
        let remote = vec!["/Users/test/mnt/mydev/home/user/.claude/projects/*/*.jsonl".to_string()];
        let local_markers = derive_path_markers(&local);
        let remote_markers = derive_path_markers(&remote);
        assert!(remote_markers[0].len() > local_markers[0].len());
    }

    #[test]
    fn test_load_config_no_ahrc() {
        // Without ~/.ahrc, should return built-in defaults
        let home = PathBuf::from("/nonexistent/home");
        let (agents, remotes) = load_config(&home);
        assert_eq!(agents.len(), 8);
        assert_eq!(agents[0].id, "claude");
        assert_eq!(agents[1].id, "codex");
        assert!(!agents[0].disabled);
        assert!(remotes.is_empty());
    }

    #[test]
    fn test_disabled_agent_does_not_fall_through_to_shorter_marker() {
        // agy (marker /.gemini/antigravity-cli/) disabled must NOT make its files
        // match gemini (marker /.gemini/).
        let home = test_home();
        let (mut agents, _) = load_config(&home);
        agents.iter_mut().find(|a| a.id == "agy").unwrap().disabled = true;
        let agy_path = Path::new(
            "/Users/test/.gemini/antigravity-cli/brain/uuid/.system_generated/logs/transcript.jsonl",
        );
        assert!(best_agent_for_path(&agents, agy_path).is_none());
        // gemini's own files still resolve to gemini
        let gemini_path = Path::new("/Users/test/.gemini/tmp/proj/chats/session-1.json");
        assert_eq!(
            best_agent_for_path(&agents, gemini_path).map(|a| a.id.as_str()),
            Some("gemini")
        );
        // and with agy enabled, the longer marker wins
        agents.iter_mut().find(|a| a.id == "agy").unwrap().disabled = false;
        assert_eq!(
            best_agent_for_path(&agents, agy_path).map(|a| a.id.as_str()),
            Some("agy")
        );
    }

    #[test]
    fn test_parse_ahrc_disabled() {
        let toml_str = r#"
[agents.codex]
disabled = true
"#;
        let config: AhrcConfig = toml::from_str(toml_str).unwrap();
        assert!(config.agents["codex"].disabled.unwrap());
    }

    #[test]
    fn test_parse_ahrc_extra_patterns() {
        let toml_str = r#"
[agents.claude]
extra_patterns = ["~/.claude-dev/projects/*/*.jsonl"]
"#;
        let config: AhrcConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.agents["claude"].extra_patterns.as_ref().unwrap()[0],
            "~/.claude-dev/projects/*/*.jsonl"
        );
    }

    #[test]
    fn test_extra_patterns_extend_path_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(
            home.join(".ahrc"),
            r#"
[agents.opencode]
extra_patterns = ["~/backup/sessions.db", "~/*.db"]
"#,
        )
        .unwrap();
        let (agents, _) = load_config(home);
        let opencode = agents.iter().find(|a| a.id == "opencode").unwrap();
        let backup = home.join("backup/sessions.db");
        assert!(opencode.matches_path(&backup.join("ses_x")));
        // A marker at HOME itself would match every agent's files.
        assert!(
            !opencode
                .path_markers
                .contains(&home.to_string_lossy().to_string())
        );
    }

    #[test]
    fn test_extra_path_marker() {
        let home = Path::new("/data/home/me");
        let others = vec![
            "/data/home/me/.claude/projects/*/*.jsonl".to_string(),
            "/data/home/me/.codex/sessions/**/*.jsonl".to_string(),
        ];
        // A literal prefix inside another agent's directory name.
        assert_eq!(
            extra_path_marker("/data/home/me/.c*.db", home, &others),
            None
        );
        assert_eq!(
            extra_path_marker("/data/home/me/.*.jsonl", home, &others),
            None
        );
        assert_eq!(
            extra_path_marker("/data/home/me/backup/sessions.db", home, &others).as_deref(),
            Some("/data/home/me/backup/sessions.db")
        );
        // An exact file directly under HOME is still distinguishable.
        assert_eq!(
            extra_path_marker("/data/home/me/opencode-backup.db", home, &others).as_deref(),
            Some("/data/home/me/opencode-backup.db")
        );
        // Globs within the file name keep the literal prefix, so agents
        // sharing a directory stay distinct.
        assert_eq!(
            extra_path_marker("/data/home/me/archive/claude-*.jsonl", home, &others).as_deref(),
            Some("/data/home/me/archive/claude-")
        );
        assert_eq!(
            extra_path_marker("/data/home/me/backup/*/x.db", home, &others).as_deref(),
            Some("/data/home/me/backup")
        );
        // Glob in the middle of a component: cut to the directory (HOME).
        assert_eq!(
            extra_path_marker("/data/home/me/.c*/x.db", home, &others),
            None
        );
        assert_eq!(extra_path_marker("/data/home/me/*.db", home, &others), None);
        // An alias of HOME (e.g. /home/me -> /data/home/me) is also rejected.
        assert_eq!(extra_path_marker("/home/me/*.jsonl", home, &others), None);
        assert_eq!(
            extra_path_marker("/tmp/archive/*/*.jsonl", home, &others).as_deref(),
            Some("/tmp/archive")
        );
    }

    #[test]
    fn test_parse_ahrc_custom_agent() {
        let toml_str = r#"
[agents.mybot]
plugin = "claude"
file_patterns = ["~/.mybot/sessions/*.jsonl"]
"#;
        let config: AhrcConfig = toml::from_str(toml_str).unwrap();
        let mybot = &config.agents["mybot"];
        assert_eq!(mybot.plugin.as_deref(), Some("claude"));
        assert_eq!(
            mybot.file_patterns.as_ref().unwrap()[0],
            "~/.mybot/sessions/*.jsonl"
        );
    }
}

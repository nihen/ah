use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::agents::{self, AgentPlugin};

static AGENT_REGISTRY: OnceLock<Vec<AgentDef>> = OnceLock::new();

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

/// TOML deserialization structures.
#[derive(Deserialize, Default)]
struct AhrcConfig {
    #[serde(default)]
    agents: HashMap<String, AgentEntry>,
}

#[derive(Deserialize, Default)]
struct AgentEntry {
    plugin: Option<String>,
    file_patterns: Option<Vec<String>>,
    extra_patterns: Option<Vec<String>>,
    disabled: Option<bool>,
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

/// Load config and build agent registry.
fn load_config(home: &Path) -> Vec<AgentDef> {
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
        return agents;
    };

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
                        Ok(expanded) => existing.glob_patterns.push(expanded),
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

    agents
}

/// Initialize the global agent registry. Call once at startup.
pub fn init(home: &Path) {
    AGENT_REGISTRY.get_or_init(|| load_config(home));
}

/// Get all agents (including disabled).
pub fn agents() -> &'static [AgentDef] {
    AGENT_REGISTRY
        .get()
        .expect("config::init() must be called before config::agents()")
}

/// Get only active (non-disabled) agents.
pub fn active_agents() -> impl Iterator<Item = &'static AgentDef> {
    agents().iter().filter(|a| !a.disabled)
}

/// Find the best matching active agent for a path.
/// Prefers the agent with the longest matching path_marker (most specific match).
pub fn find_agent_for_path(path: &Path) -> Option<&'static AgentDef> {
    let path_str = path.to_string_lossy();
    active_agents()
        .filter(|a| a.matches_path(path))
        .max_by_key(|a| {
            a.path_markers
                .iter()
                .filter(|m| path_str.contains(m.as_str()))
                .map(|m| m.len())
                .max()
                .unwrap_or(0)
        })
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
        let agents = load_config(&home);
        assert_eq!(agents.len(), 5);
        assert_eq!(agents[0].id, "claude");
        assert_eq!(agents[1].id, "codex");
        assert!(!agents[0].disabled);
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
    fn test_parse_ahrc_custom_agent() {
        let toml_str = r#"
[agents.aider]
plugin = "claude"
file_patterns = ["~/.aider/history/*.jsonl"]
"#;
        let config: AhrcConfig = toml::from_str(toml_str).unwrap();
        let aider = &config.agents["aider"];
        assert_eq!(aider.plugin.as_deref(), Some("claude"));
        assert_eq!(
            aider.file_patterns.as_ref().unwrap()[0],
            "~/.aider/history/*.jsonl"
        );
    }
}

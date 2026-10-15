//! `ah list-agents`: the configured agents and their capabilities.
use crate::cli;
use crate::config;

fn caps(agent: &config::AgentDef) -> Vec<&'static str> {
    let p = agent.plugin;
    let mut v = Vec::new();
    if p.can_search() {
        v.push("search");
    }
    if p.can_show() {
        v.push("show");
    }
    if p.can_resume() {
        v.push("resume");
    }
    if p.can_detect_running() {
        v.push("running");
    }
    // Memory files are found through the built-in agent's own
    // locations, so a custom alias of the same plugin lists none.
    if agent.is_builtin && p.can_memory() {
        v.push("memory");
    }
    v
}

/// One agent definition as the JSON object `list-agents --json` prints.
pub fn agent_def_to_json(agent: &config::AgentDef) -> serde_json::Value {
    serde_json::json!({
        "id": agent.id,
        "description": agent.description,
        "capabilities": caps(agent),
        "patterns": agent.glob_patterns,
        "subagent_patterns": agent.subagent_patterns,
        "project_desc": agent.plugin.project_desc(),
    })
}

/// Every configured agent as JSON, with `disabled` and `builtin` added.
pub fn agent_defs_to_json() -> Vec<serde_json::Value> {
    config::agents()
        .iter()
        .map(|agent| {
            let mut obj = agent_def_to_json(agent);
            if let Some(map) = obj.as_object_mut() {
                map.insert("disabled".into(), agent.disabled.into());
                map.insert("builtin".into(), agent.is_builtin.into());
            }
            obj
        })
        .collect()
}

pub fn run(fmt: cli::OutputFormat) {
    let all = config::agents();
    match fmt {
        cli::OutputFormat::Json => {
            for agent in all {
                println!("{}", agent_def_to_json(agent));
            }
        }
        cli::OutputFormat::Tsv => {
            for agent in all {
                println!(
                    "{}\t{}\t{}\t{}",
                    agent.id,
                    agent.description,
                    caps(agent).join(","),
                    agent.glob_patterns.join(","),
                );
            }
        }
        cli::OutputFormat::Ltsv => {
            for agent in all {
                println!(
                    "id:{}\tdescription:{}\tcapabilities:{}\tpatterns:{}",
                    agent.id,
                    agent.description,
                    caps(agent).join(","),
                    agent.glob_patterns.join(","),
                );
            }
        }
        cli::OutputFormat::Log | cli::OutputFormat::Table => {
            let builtins: Vec<_> = all.iter().filter(|a| a.is_builtin).collect();
            let custom: Vec<_> = all.iter().filter(|a| !a.is_builtin).collect();

            println!("Built-in agents:");
            println!();
            for agent in &builtins {
                let status = if agent.disabled { " (disabled)" } else { "" };
                println!("{:<12} {}{}", agent.id, agent.description, status);
                println!(
                    "             Patterns:     {}",
                    agent.glob_patterns.join(", ")
                );
                println!("             Capabilities: {}", caps(agent).join(", "));
                println!("             Project:      {}", agent.plugin.project_desc());
                println!();
            }

            if !custom.is_empty() {
                println!("Custom agents (~/.ahrc):");
                println!();
                for agent in &custom {
                    let status = if agent.disabled { " (disabled)" } else { "" };
                    println!("{:<12} plugin: {}{}", agent.id, agent.plugin.id(), status);
                    println!(
                        "             Patterns:     {}",
                        agent.glob_patterns.join(", ")
                    );
                    println!("             Capabilities: {}", caps(agent).join(", "));
                    println!();
                }
            }
        }
    }
}

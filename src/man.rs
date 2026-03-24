use std::io::Write;

use clap::CommandFactory;
use clap_mangen::Man;
use clap_mangen::roff::{Roff, bold, italic, roman};

use crate::cli::Cli;

/// Example entry: (command, description)
pub const EXAMPLES: &[(&str, &str)] = &[
    ("ah log", "List sessions for the current directory"),
    ("ah log -a -q \"auth\"", "Search across all sessions"),
    ("ah log -a --since 3d", "Sessions from the last 3 days"),
    (
        "ah log -o agent,title,id --tsv",
        "Custom fields in TSV format",
    ),
    ("ah show", "Show the latest session transcript"),
    ("ah show -q \"OAuth\"", "Show the latest matching session"),
    ("ah show -i", "Interactively select and show a session"),
    ("ah resume", "Resume the latest matching session"),
    ("ah resume -i", "Browse sessions with fzf/sk and resume"),
    (
        "ah resume -- --dry-run",
        "Pass extra args to the agent command",
    ),
    ("ah project", "List all known projects with session counts"),
    ("ah memory", "List agent memory and instruction files"),
    ("ah agent", "Show session summary per agent"),
    ("ah list-agents", "List supported agents and capabilities"),
];

/// Generate all man pages to `w`. If `subcommand` is Some, generate only that one.
pub fn generate(w: &mut dyn Write, subcommand: Option<&str>) -> Result<(), std::io::Error> {
    let cmd = Cli::command();

    match subcommand {
        None => render_main(w, &cmd)?,
        Some(name) => {
            let sub = cmd
                .get_subcommands()
                .find(|s| s.get_name() == name)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("unknown subcommand: {name}"),
                    )
                })?;
            let mut sub = sub.clone();
            sub.build();
            let display = format!("ah-{name}");
            let man = Man::new(sub).title(&display);
            man.render(w)?;
        }
    }
    Ok(())
}

/// Generate all man page files into a directory.
pub fn generate_all(out_dir: &std::path::Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(out_dir)?;

    // Main page
    let mut f = std::fs::File::create(out_dir.join("ah.1"))?;
    generate(&mut f, None)?;
    f.flush()?;

    // Subcommand pages
    let cmd = Cli::command();
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let name = sub.get_name();
        // Skip utility subcommands that don't need their own man page
        if name == "man" || name == "help" || name == "completion" {
            continue;
        }
        let mut f = std::fs::File::create(out_dir.join(format!("ah-{name}.1")))?;
        generate(&mut f, Some(name))?;
        f.flush()?;
    }
    Ok(())
}

fn render_main(w: &mut dyn Write, cmd: &clap::Command) -> Result<(), std::io::Error> {
    let man = Man::new(cmd.clone());

    // Use individual section renderers to interleave custom sections.
    man.render_title(w)?;
    man.render_name_section(w)?;
    man.render_synopsis_section(w)?;

    // Enhanced DESCRIPTION
    render_description(w)?;

    man.render_options_section(w)?;
    man.render_subcommands_section(w)?;

    render_examples(w)?;
    render_configuration(w)?;
    render_environment(w)?;
    render_exit_status(w)?;
    render_see_also(w)?;

    man.render_version_section(w)?;

    Ok(())
}

fn render_description(w: &mut dyn Write) -> Result<(), std::io::Error> {
    let mut roff = Roff::default();
    roff.control("SH", ["DESCRIPTION"]);
    roff.text([roman(
        "ah (Agent History) is a unified CLI for searching, inspecting, and resuming \
         coding-agent sessions across multiple AI agents (Claude Code, Codex CLI, \
         Gemini CLI, Cursor Agent, and more).",
    )]);
    roff.control("PP", []);
    roff.text([
        roman(
            "By default, all subcommands filter sessions to the current working directory. \
         Use ",
        ),
        bold("-a"),
        roman(" to search across all known sessions."),
    ]);
    roff.control("PP", []);
    roff.text([
        roman("All subcommands except "),
        bold("resume"),
        roman(" are read-only. The "),
        bold("resume"),
        roman(" command is the only one that launches an agent process."),
    ]);
    roff.to_writer(w)
}

fn render_examples(w: &mut dyn Write) -> Result<(), std::io::Error> {
    let mut roff = Roff::default();
    roff.control("SH", ["EXAMPLES"]);

    for (cmd, desc) in EXAMPLES {
        roff.text([roman(*desc)]);
        roff.control("PP", []);
        roff.control("RS", ["4"]);
        roff.text([bold(*cmd)]);
        roff.control("RE", []);
        roff.control("PP", []);
    }

    roff.to_writer(w)
}

fn render_configuration(w: &mut dyn Write) -> Result<(), std::io::Error> {
    let mut roff = Roff::default();
    roff.control("SH", ["CONFIGURATION"]);
    roff.text([
        roman("Optional configuration file: "),
        bold("~/.ahrc"),
        roman(" (TOML format)."),
    ]);
    roff.control("PP", []);
    roff.text([roman(
        "Without a config file, all built-in agents are active with default settings. \
         The config file allows:",
    )]);
    roff.control("PP", []);
    roff.control("RS", ["4"]);
    roff.text([roman("- Adding custom agents with custom file patterns")]);
    roff.control("br", []);
    roff.text([roman("- Disabling built-in agents")]);
    roff.control("br", []);
    roff.text([roman("- Adding extra glob patterns to existing agents")]);
    roff.control("RE", []);
    roff.control("PP", []);
    roff.text([
        roman("See "),
        bold("ah list-agents"),
        roman(" to view the current agent configuration."),
    ]);
    roff.to_writer(w)
}

fn render_environment(w: &mut dyn Write) -> Result<(), std::io::Error> {
    let mut roff = Roff::default();
    roff.control("SH", ["ENVIRONMENT"]);

    let vars: &[(&str, &str)] = &[
        (
            "AH_SELECTOR",
            "Override the default fuzzy selector command for interactive mode (default: fzf)",
        ),
        (
            "CLAUDE_CONFIG_DIR",
            "Override the base directory for Claude Code session files",
        ),
        (
            "NO_COLOR",
            "Disable colored output (see https://no-color.org/)",
        ),
    ];

    for (var, desc) in vars {
        roff.control("TP", []);
        roff.text([bold(*var)]);
        roff.text([roman(*desc)]);
    }

    roff.to_writer(w)
}

fn render_exit_status(w: &mut dyn Write) -> Result<(), std::io::Error> {
    let mut roff = Roff::default();
    roff.control("SH", ["EXIT STATUS"]);
    roff.control("TP", []);
    roff.text([bold("0")]);
    roff.text([roman("Success")]);
    roff.control("TP", []);
    roff.text([bold("1")]);
    roff.text([roman("No sessions found, or an error occurred")]);
    roff.to_writer(w)
}

fn render_see_also(w: &mut dyn Write) -> Result<(), std::io::Error> {
    let mut roff = Roff::default();
    roff.control("SH", ["SEE ALSO"]);

    let subcmds = [
        "ah-log",
        "ah-show",
        "ah-resume",
        "ah-project",
        "ah-agent",
        "ah-memory",
        "ah-list-agents",
    ];
    let refs: Vec<String> = subcmds.iter().map(|s| format!("{s}(1)")).collect();
    let joined = refs.join(", ");
    roff.text([roman(&joined)]);

    roff.control("PP", []);
    roff.text([italic("https://github.com/nihen/ah")]);

    roff.to_writer(w)
}

use assert_cmd::Command;

fn ah() -> Command {
    Command::cargo_bin("ah").unwrap()
}

fn help_output(args: &[&str]) -> String {
    let output = ah().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).unwrap()
}

// ─── Help snapshot tests ─────────────────────────────────────────

#[test]
fn snapshot_help_top() {
    insta::assert_snapshot!(help_output(&["--help"]));
}

#[test]
fn snapshot_help_log() {
    insta::assert_snapshot!(help_output(&["log", "--help"]));
}

#[test]
fn snapshot_help_search() {
    insta::assert_snapshot!(help_output(&["search", "--help"]));
}

#[test]
fn snapshot_help_show() {
    insta::assert_snapshot!(help_output(&["show", "--help"]));
}

#[test]
fn snapshot_help_resume() {
    insta::assert_snapshot!(help_output(&["resume", "--help"]));
}

#[test]
fn snapshot_help_project() {
    insta::assert_snapshot!(help_output(&["project", "--help"]));
}

#[test]
fn snapshot_help_memory() {
    insta::assert_snapshot!(help_output(&["memory", "--help"]));
}

#[test]
fn snapshot_help_agent() {
    insta::assert_snapshot!(help_output(&["agent", "--help"]));
}

#[test]
fn snapshot_help_list_agents() {
    insta::assert_snapshot!(help_output(&["list-agents", "--help"]));
}

#[test]
fn snapshot_help_completion() {
    insta::assert_snapshot!(help_output(&["completion", "--help"]));
}

// ─── README snapshot test ────────────────────────────────────────

#[test]
fn snapshot_readme() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("README.md should exist");
    insta::assert_snapshot!(readme);
}

#[test]
fn readme_search_and_show_help_match_cli() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("README.md should exist");
    for command in ["search", "show"] {
        let heading = format!("### `ah {command} --help`");
        let section = readme.split_once(&heading).expect("help section").1;
        let fenced = section.split_once("\n```\n").expect("help fence").1;
        let documented = fenced.split_once("\n```").expect("closing fence").0;
        assert_eq!(documented, help_output(&[command, "--help"]).trim_end());
    }
}

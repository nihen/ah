use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn ah() -> Command {
    Command::cargo_bin("ah").unwrap()
}

fn fixture_path(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .display()
        .to_string()
}

fn codex_session_copy() -> (TempDir, String) {
    let tmp = TempDir::new().unwrap();
    let session_path = tmp
        .path()
        .join(".codex/sessions/2026/03/24/rollout-2026-03-24T20-43-12-codex-sess-001.jsonl");
    fs::create_dir_all(session_path.parent().unwrap()).unwrap();
    fs::copy(fixture_path("codex_session.jsonl"), &session_path).unwrap();
    (tmp, session_path.display().to_string())
}

// ─── Basic functionality ───────────────────────────────────────────

#[test]
fn version_flag() {
    ah().arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("ah "));
}

#[test]
fn help_flag() {
    ah().arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage"))
        .stdout(predicate::str::contains("log"));
}

#[test]
fn log_help() {
    ah().args(["log", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("List sessions"));
}

#[test]
fn show_help() {
    ah().args(["show", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Show session transcript"));
}

#[test]
fn resume_help_includes_print() {
    ah().args(["resume", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--print"))
        .stdout(predicate::str::contains("read-only"));
}

#[test]
fn list_agents_shows_builtin_agents() {
    ah().arg("list-agents")
        .assert()
        .success()
        .stdout(predicate::str::contains("claude"))
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("gemini"))
        .stdout(predicate::str::contains("copilot"))
        .stdout(predicate::str::contains("cursor"));
}

#[test]
fn list_agents_json() {
    let output = ah().args(["list-agents", "--json"]).assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    // Each line should be valid JSON
    for line in stdout.lines() {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Invalid JSON line: {e}\nLine: {line}"));
        assert!(parsed.get("id").is_some(), "JSON missing 'id' field");
    }
}

#[test]
fn list_agents_tsv() {
    let output = ah().args(["list-agents", "--tsv"]).assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        assert!(
            fields.len() >= 2,
            "TSV line should have at least 2 tab-separated fields, got: {line}"
        );
    }
}

// ─── Aliases ───────────────────────────────────────────────────────

#[test]
fn alias_search_help() {
    ah().args(["search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("List sessions"));
}

#[test]
fn alias_cat_help() {
    ah().args(["cat", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Show session transcript"));
}

#[test]
fn alias_projects_help() {
    ah().args(["projects", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("project"));
}

// ─── Output formats ───────────────────────────────────────────────

#[test]
fn log_tsv_output() {
    // May exit 0 (sessions found) or 1 (no sessions on CI), both are valid
    let output = ah()
        .args(["log", "-a", "-n", "1", "--tsv"])
        .output()
        .unwrap();
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "unexpected exit code: {:?}",
        output.status
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    if !stdout.is_empty() {
        for line in stdout.lines() {
            assert!(
                line.contains('\t'),
                "TSV output should contain tabs: {line}"
            );
        }
    }
}

#[test]
fn log_json_output() {
    let output = ah()
        .args(["log", "-a", "-n", "1", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "unexpected exit code: {:?}",
        output.status
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    if !stdout.is_empty() {
        for line in stdout.lines() {
            let _: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("Invalid JSON: {e}\nLine: {line}"));
        }
    }
}

#[test]
fn log_ltsv_output() {
    let output = ah()
        .args(["log", "-a", "-n", "1", "--ltsv"])
        .output()
        .unwrap();
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "unexpected exit code: {:?}",
        output.status
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    if !stdout.is_empty() {
        for line in stdout.lines() {
            // LTSV lines contain key:value pairs separated by tabs
            assert!(
                line.contains(':'),
                "LTSV output should contain key:value pairs: {line}"
            );
        }
    }
}

// ─── Filter options ───────────────────────────────────────────────

#[test]
fn log_nonexistent_agent_filter() {
    ah().args(["log", "-a", "--agent", "nonexistent", "-n", "1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No sessions found"));
}

#[test]
fn log_invalid_since_spec() {
    // "99y" is not a valid time spec (y suffix not supported)
    ah().args(["log", "-a", "--since", "99y", "-n", "1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid time spec"));
}

// ─── Error handling ───────────────────────────────────────────────

#[test]
fn show_nonexistent_path() {
    // /nonexistent/path.jsonl looks like a file path, so it won't try ID resolution
    ah().args(["show", "/nonexistent/path.jsonl"])
        .assert()
        .failure();
}

#[test]
fn show_highlight_emits_ansi_with_color() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "--color", "--highlight", "redis", &session_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[7m"));
}

#[test]
fn show_highlight_no_ansi_without_color() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "--no-color", "--highlight", "redis", &session_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[7m").not());
}

#[test]
fn show_highlight_conflicts_with_json() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "--json", "--highlight", "redis", &session_path])
        .assert()
        .failure();
}

#[test]
fn resume_print_outputs_command_without_executing() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["resume", "--print", &session_path])
        .assert()
        .success()
        .stdout(predicate::eq(
            "cd '/Users/test/api-server' && 'codex' 'resume' 'codex-sess-001'\n",
        ));
}

#[test]
fn resume_print_appends_extra_args() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["resume", "--print", &session_path, "--", "--model", "gpt-5"])
        .assert()
        .success()
        .stdout(predicate::eq(
            "cd '/Users/test/api-server' && 'codex' 'resume' 'codex-sess-001' '--model' 'gpt-5'\n",
        ));
}

#[test]
fn log_invalid_regex() {
    ah().args(["log", "-a", "-q", "[invalid"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid regex"));
}

// ─── Field list ───────────────────────────────────────────────────

#[test]
fn log_field_list() {
    ah().args(["log", "--list-fields"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent"))
        .stdout(predicate::str::contains("path"))
        .stdout(predicate::str::contains("title"));
}

#[test]
fn project_field_list() {
    ah().args(["project", "--list-fields"])
        .assert()
        .success()
        .stdout(predicate::str::contains("project"))
        .stdout(predicate::str::contains("agents"));
}

#[test]
fn memory_field_list() {
    ah().args(["memory", "--list-fields"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent"))
        .stdout(predicate::str::contains("path"));
}

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
        .stdout(predicate::str::contains("\x1b[30;103m"));
}

#[test]
fn show_highlight_no_ansi_without_color() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "--no-color", "--highlight", "redis", &session_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[30;103m").not());
}

#[test]
fn show_highlight_conflicts_with_json() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "--json", "--highlight", "redis", &session_path])
        .assert()
        .failure();
}

#[test]
fn show_meta_conflicts_with_raw() {
    ah().args(["show", "-o", "title", "--raw"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "-o/--fields cannot be combined with --raw/--json/--md/--pretty",
        ));
}

#[test]
fn show_meta_conflicts_with_head() {
    ah().args(["show", "-o", "title", "--head", "1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "-o/--fields and --tsv cannot be combined with --head",
        ));
}

#[test]
fn show_meta_conflicts_with_pretty() {
    ah().args(["show", "-o", "title", "--pretty"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot be combined with --raw/--json/--md/--pretty",
        ));
}

#[test]
fn show_meta_conflicts_with_follow() {
    ah().args(["show", "--tsv", "--follow"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be combined with --follow"));
}

#[test]
fn show_meta_conflicts_with_highlight() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "-o", "title", "--highlight", "redis", &session_path])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot be combined with --highlight",
        ));
}

#[test]
fn show_tsv_conflicts_with_highlight() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "--tsv", "--highlight", "redis", &session_path])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot be combined with --highlight",
        ));
}

#[test]
fn interactive_display_rejected_for_resume() {
    ah().args(["resume", "-i", "--interactive-display", "title"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--interactive-display is only supported",
        ));
}

#[test]
fn interactive_display_rejects_path_field() {
    ah().args(["log", "-i", "--interactive-display", "path"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot include `path`"));
}

#[cfg(unix)]
#[test]
fn log_interactive_with_o_emits_field_tsv_after_selection() {
    // Use a shell-script "fake selector" that ignores all flags fzf would
    // normally receive, reads the candidate list from stdin, and echoes the
    // first line — i.e. simulates the user pressing Enter on the top entry.
    // After "selection", print_session_fields re-resolves the fixture session
    // and emits the requested fields as TSV (here `agent,id`, since both are
    // cheap to resolve and `agent` exercises the plugin classification).
    let (_tmp, session_path) = codex_session_copy();
    let tmp_root = std::path::Path::new(&session_path)
        .ancestors()
        .nth(6)
        .expect("session_path should have ${tmp}/.codex/sessions/Y/M/D/file.jsonl shape")
        .to_path_buf();

    let selector_script = tmp_root.join("fake-selector.sh");
    fs::write(
        &selector_script,
        "#!/bin/sh\nIFS= read -r line\necho \"$line\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&selector_script, fs::Permissions::from_mode(0o755)).unwrap();

    let assert = ah()
        .env("HOME", &tmp_root)
        .env("CLAUDE_CONFIG_DIR", "/nonexistent")
        .env("GEMINI_CLI_HOME", "/nonexistent")
        .env("COPILOT_HOME", "/nonexistent")
        .env("CURSOR_CONFIG_DIR", "/nonexistent")
        .args([
            "log",
            "-a",
            "-i",
            "-s",
            selector_script.to_str().unwrap(),
            "-o",
            "agent,id",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let line = stdout.trim_end_matches('\n');
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(cols.len(), 2, "expected 2 TSV cols, got {:?}", cols);
    assert_eq!(cols[0], "codex");
    assert_eq!(cols[1], "codex-sess-001");
}

#[test]
fn show_meta_outputs_title_for_fixture() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", &session_path, "-o", "title"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn show_meta_outputs_tab_separated_fields() {
    let (_tmp, session_path) = codex_session_copy();
    let assert = ah()
        .args(["show", &session_path, "-o", "agent,id"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let line = stdout.trim_end_matches('\n');
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(
        cols.len(),
        2,
        "expected 2 TSV columns, got {}: {:?}",
        cols.len(),
        cols
    );
    assert_eq!(cols[0], "codex");
    assert_eq!(cols[1], "codex-sess-001");
}

#[test]
fn show_meta_hoists_path_to_first_column() {
    let (_tmp, session_path) = codex_session_copy();
    let assert = ah()
        .args(["show", &session_path, "-o", "id,path"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let line = stdout.trim_end_matches('\n');
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(cols.len(), 2);
    // hoist_path_first reorders so path comes first regardless of -o order.
    assert_eq!(cols[0], session_path);
    assert_eq!(cols[1], "codex-sess-001");
}

#[test]
fn show_tsv_default_emits_title() {
    // The codex fixture extracts the title from the first user prompt.
    // Asserting the actual title value verifies the "default field is
    // title" contract instead of just "non-empty output".
    let (_tmp, session_path) = codex_session_copy();
    let assert = ah()
        .args(["show", &session_path, "--tsv"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout.trim_end_matches('\n'), "add redis caching");
}

#[test]
fn show_meta_rejects_empty_field_list() {
    ah().args(["show", "-o", ""])
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires at least one field"));
}

#[test]
fn show_meta_rejects_missing_session_file() {
    ah().args(["show", "/nonexistent/sess.jsonl", "-o", "title"])
        .assert()
        .failure();
}

/// Helper for `--interactive-display` tests: builds a fake selector script
/// that simulates the user pressing Enter on the first candidate by echoing
/// the first line of stdin, AND saves the full input to a side file so the
/// test can inspect what columns the picker actually saw.
#[cfg(unix)]
fn build_capture_selector(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("capture-selector.sh");
    let captured = dir.join("captured-input.txt");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat > '{}'\nhead -n1 '{}'\n",
            captured.display(),
            captured.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn log_interactive_display_overrides_picker_columns() {
    let (_tmp, session_path) = codex_session_copy();
    let tmp_root = std::path::Path::new(&session_path)
        .ancestors()
        .nth(6)
        .unwrap()
        .to_path_buf();
    let selector = build_capture_selector(&tmp_root);
    let captured = tmp_root.join("captured-input.txt");

    ah().env("HOME", &tmp_root)
        .env("CLAUDE_CONFIG_DIR", "/nonexistent")
        .env("GEMINI_CLI_HOME", "/nonexistent")
        .env("COPILOT_HOME", "/nonexistent")
        .env("CURSOR_CONFIG_DIR", "/nonexistent")
        .args([
            "log",
            "-a",
            "-i",
            "-s",
            selector.to_str().unwrap(),
            "--interactive-display",
            "agent",
            "--no-preview",
        ])
        .assert()
        .success();

    let input = fs::read_to_string(&captured).unwrap();
    // With --interactive-display=agent, the picker should show ONLY the
    // `agent` column (no project / modified_at / title from the default).
    assert!(
        input.contains("codex"),
        "picker input should contain `agent` column value: {:?}",
        input
    );
    // The fixture's project resolves to "api-server" (from cwd extraction);
    // the default display fallback would include it as a column.
    assert!(
        !input.contains("api-server"),
        "picker input should NOT contain default project column: {:?}",
        input
    );
}

#[cfg(unix)]
#[test]
fn show_interactive_display_allows_matched() {
    // `--interactive-display matched` is rejected for `ah log -i` (whose
    // picker uses display-only resolve opts) but allowed for `ah show -i`
    // (which uses query-aware opts). This test verifies the show-side path
    // accepts `matched` and includes the query value in the picker columns.
    let (_tmp, session_path) = codex_session_copy();
    let tmp_root = std::path::Path::new(&session_path)
        .ancestors()
        .nth(6)
        .unwrap()
        .to_path_buf();
    let selector = build_capture_selector(&tmp_root);
    let captured = tmp_root.join("captured-input.txt");

    ah().env("HOME", &tmp_root)
        .env("CLAUDE_CONFIG_DIR", "/nonexistent")
        .env("GEMINI_CLI_HOME", "/nonexistent")
        .env("COPILOT_HOME", "/nonexistent")
        .env("CURSOR_CONFIG_DIR", "/nonexistent")
        .args([
            "show",
            "-a",
            "-i",
            "-s",
            selector.to_str().unwrap(),
            "--interactive-display",
            "matched",
            "-q",
            "redis",
            "--no-preview",
        ])
        .assert()
        .success();
    let input = fs::read_to_string(&captured).unwrap();
    // The query happens to also appear in the fixture's title ("add redis
    // caching"), so a `contains("redis")` check could pass even if the
    // picker fell back to default columns. Assert instead that the default
    // `project` column ("api-server") is absent — proves --interactive-display
    // actually overrode the columns and only `matched` was emitted.
    assert!(
        input.contains("redis"),
        "show -i --interactive-display matched -q redis should display the matched snippet: {:?}",
        input
    );
    assert!(
        !input.contains("api-server"),
        "picker input should NOT contain default project column when \
         --interactive-display=matched is set: {:?}",
        input
    );
}

#[test]
fn log_interactive_display_rejects_matched() {
    // Symmetric to show_interactive_display_allows_matched: log's picker
    // doesn't use query-aware resolve opts, so matched is rejected upfront.
    ah().args(["log", "-a", "-i", "--interactive-display", "matched"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("for `ah log -i`"));
}

#[cfg(unix)]
#[test]
fn show_interactive_with_o_emits_field_tsv_after_selection() {
    // Mirror of `log_interactive_with_o_emits_field_tsv_after_selection` but
    // for the `show -i` post-selection path (which goes through
    // query-aware ResolveOpts in run_show + emit_session_meta_tsv).
    let (_tmp, session_path) = codex_session_copy();
    let tmp_root = std::path::Path::new(&session_path)
        .ancestors()
        .nth(6)
        .unwrap()
        .to_path_buf();
    let selector_script = tmp_root.join("show-fake-selector.sh");
    fs::write(
        &selector_script,
        "#!/bin/sh\nIFS= read -r line\necho \"$line\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&selector_script, fs::Permissions::from_mode(0o755)).unwrap();

    let assert = ah()
        .env("HOME", &tmp_root)
        .env("CLAUDE_CONFIG_DIR", "/nonexistent")
        .env("GEMINI_CLI_HOME", "/nonexistent")
        .env("COPILOT_HOME", "/nonexistent")
        .env("CURSOR_CONFIG_DIR", "/nonexistent")
        .args([
            "show",
            "-a",
            "-i",
            "-s",
            selector_script.to_str().unwrap(),
            "-o",
            "agent,id",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let line = stdout.trim_end_matches('\n');
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(cols.len(), 2, "expected 2 TSV cols, got {:?}", cols);
    assert_eq!(cols[0], "codex");
    assert_eq!(cols[1], "codex-sess-001");
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

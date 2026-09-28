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
fn list_agents_advertises_memory_for_builtin_agents_only() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    fs::write(
        home.join(".ahrc"),
        "[agents.mybot]\nplugin = \"gemini\"\nfile_patterns = [\"~/.mybot/*.json\"]\n",
    )
    .unwrap();
    let output = ah()
        .env("HOME", &home)
        .args(["list-agents", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let caps = |id: &str| -> Vec<String> {
        stdout
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|v| v["id"] == id)
            .unwrap()["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect()
    };
    assert!(caps("gemini").contains(&"memory".to_string()));
    assert!(!caps("mybot").contains(&"memory".to_string()));
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
fn search_help_is_not_log_alias() {
    ah().args(["search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Show matching passages"));
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
        .env("CURSOR_DATA_DIR", "/nonexistent")
        .env_remove("XDG_DATA_HOME")
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
        .env("CURSOR_DATA_DIR", "/nonexistent")
        .env_remove("XDG_DATA_HOME")
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
        .env("CURSOR_DATA_DIR", "/nonexistent")
        .env_remove("XDG_DATA_HOME")
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
        .env("CURSOR_DATA_DIR", "/nonexistent")
        .env_remove("XDG_DATA_HOME")
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

/// Cursor's project directory name for `path`: runs of non-alphanumerics
/// become one `-`, with leading and trailing dashes trimmed.
fn cursor_slug(path: &std::path::Path) -> String {
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

#[test]
fn cursor_session_matches_cwd_filter_and_resumes_interactively() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let project = home.join("work/my_proj.v2");
    fs::create_dir_all(&project).unwrap();
    let transcripts = home
        .join(".cursor/projects")
        .join(cursor_slug(&project))
        .join("agent-transcripts/sess-1");
    fs::create_dir_all(&transcripts).unwrap();
    fs::copy(
        fixture_path("cursor_session.jsonl"),
        transcripts.join("sess-1.jsonl"),
    )
    .unwrap();

    let run = |args: &[&str]| {
        let assert = ah()
            .current_dir(&project)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", "/nonexistent")
            .env("CODEX_HOME", "/nonexistent")
            .env("GEMINI_CLI_HOME", "/nonexistent")
            .env("COPILOT_HOME", "/nonexistent")
            .env("CURSOR_DATA_DIR", home.join(".cursor"))
            .env_remove("XDG_DATA_HOME")
            .args(args)
            .write_stdin("")
            .assert()
            .success();
        String::from_utf8(assert.get_output().stdout.clone()).unwrap()
    };

    assert_eq!(
        run(&["log", "-o", "agent,project,id", "--tsv"]),
        "cursor\tmy_proj.v2\tsess-1\n"
    );
    assert_eq!(
        run(&["resume", "--print"]),
        format!(
            "cd '{}' && 'cursor-agent' '--resume' 'sess-1'\n",
            project.display()
        )
    );
}

#[test]
fn cursor_sessions_follow_cursor_data_dir_not_config_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let project = home.join("work/app");
    fs::create_dir_all(&project).unwrap();
    let place = |base: &std::path::Path, id: &str| {
        let dir = base
            .join("projects")
            .join(cursor_slug(&project))
            .join("agent-transcripts")
            .join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(
            fixture_path("cursor_session.jsonl"),
            dir.join(format!("{id}.jsonl")),
        )
        .unwrap();
    };
    let data_dir = home.join("cursor-data");
    let config_dir = home.join("cursor-config");
    place(&data_dir, "in-data-dir");
    place(&config_dir, "in-config-dir");
    place(&home.join(".cursor"), "in-default-dir");

    let assert = ah()
        .current_dir(&project)
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", "/nonexistent")
        .env("CODEX_HOME", "/nonexistent")
        .env("GEMINI_CLI_HOME", "/nonexistent")
        .env("COPILOT_HOME", "/nonexistent")
        .env("CURSOR_DATA_DIR", &data_dir)
        .env("CURSOR_CONFIG_DIR", &config_dir)
        .env_remove("XDG_DATA_HOME")
        .args(["log", "-o", "agent,id", "--tsv"])
        .write_stdin("")
        .assert()
        .success();
    assert_eq!(
        String::from_utf8(assert.get_output().stdout.clone()).unwrap(),
        "cursor\tin-data-dir\n"
    );
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

// ─── opencode (SQLite) ─────────────────────────────────────────────

/// Create `$HOME/.local/share/opencode/opencode.db` with one session.
fn opencode_home() -> TempDir {
    let tmp = TempDir::new().unwrap();
    // ah canonicalizes HOME (e.g. macOS /var -> /private/var).
    let db = fs::canonicalize(tmp.path())
        .unwrap()
        .join(".local/share/opencode/opencode.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL,
            title TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, data TEXT NOT NULL);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
            session_id TEXT NOT NULL, data TEXT NOT NULL);
        INSERT INTO session VALUES
            ('ses_oc1', NULL, '/nonexistent/proj', 'Fix the parser', 1700000000000, 1700000100000);
        INSERT INTO message VALUES
            ('msg_1', 'ses_oc1', 1, '{"role":"user"}'),
            ('msg_2', 'ses_oc1', 2, '{"role":"assistant"}');
        INSERT INTO part VALUES
            ('prt_1', 'msg_1', 'ses_oc1', '{"type":"text","text":"please fix the parser"}'),
            ('prt_2', 'msg_2', 'ses_oc1', '{"type":"tool","state":{"output":"opencode-needle"}}'),
            ('prt_3', 'msg_2', 'ses_oc1', '{"type":"text","text":"done"}');
        "#,
    )
    .unwrap();
    tmp
}

fn ah_opencode(home: &Path) -> Command {
    let mut cmd = ah();
    cmd.env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env("CLAUDE_CONFIG_DIR", "/nonexistent")
        .env("CODEX_HOME", "/nonexistent")
        .env("GEMINI_CLI_HOME", "/nonexistent")
        .env("COPILOT_HOME", "/nonexistent")
        .env("CURSOR_DATA_DIR", "/nonexistent")
        .env("GROK_HOME", "/nonexistent");
    cmd
}

#[test]
fn opencode_log_search_show_and_resume() {
    let tmp = opencode_home();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let home = home.as_path();
    let session_path = home.join(".local/share/opencode/opencode.db/ses_oc1");
    let session_path = session_path.to_str().unwrap();

    ah_opencode(home)
        .args(["log", "-a", "-o", "agent,id,title,path"])
        .assert()
        .success()
        .stdout(format!(
            "opencode\tses_oc1\tFix the parser\t{}\n",
            session_path
        ));

    // Full-text search covers tool output stored in the part table.
    ah_opencode(home)
        .args(["log", "-a", "-q", "opencode-needle", "-o", "id"])
        .assert()
        .success()
        .stdout("ses_oc1\n");

    ah_opencode(home)
        .args(["show", session_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("please fix the parser"))
        .stdout(predicate::str::contains("done"));

    ah_opencode(home)
        .args(["show", "--raw", "ses_oc1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"opencode-needle\""));

    ah_opencode(home)
        .args(["resume", "--print", "ses_oc1"])
        .assert()
        .success()
        .stdout("cd '/nonexistent/proj' && 'opencode' '--session' 'ses_oc1'\n");
}

#[test]
fn opencode_respects_xdg_data_home() {
    let tmp = opencode_home();
    let data_home = tmp.path().join(".local/share");
    let other_home = TempDir::new().unwrap();
    ah_opencode(other_home.path())
        .env("XDG_DATA_HOME", &data_home)
        .args(["log", "-a", "-o", "agent,id"])
        .assert()
        .success()
        .stdout("opencode\tses_oc1\n");
}

#[test]
fn opencode_duplicate_session_keeps_newest_copy() {
    let tmp = opencode_home();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let backup = home.join(".local/share/opencode/opencode-backup.db");
    let conn = rusqlite::Connection::open(&backup).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL,
            title TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
        INSERT INTO session VALUES
            ('ses_oc1', NULL, '/nonexistent/proj', 'Old title', 1700000000000, 1600000000000);
        "#,
    )
    .unwrap();
    drop(conn);

    // The newest copy wins regardless of the sort order.
    for sort in [
        &[][..],
        &["--asc"],
        &["-S", "title"],
        &["-S", "title", "--asc"],
    ] {
        ah_opencode(&home)
            .args(["log", "-a", "-o", "agent,id,title"])
            .args(sort)
            .assert()
            .success()
            .stdout("opencode\tses_oc1\tFix the parser\n");
    }
    // Prefix lookup is not ambiguous.
    ah_opencode(&home)
        .args(["show", "-o", "title", "ses_oc"])
        .assert()
        .success()
        .stdout("Fix the parser\n");
}

#[test]
fn opencode_unattributable_extra_copy_does_not_hide_sessions() {
    // A same-age copy directly under HOME via an overly broad extra pattern
    // must not replace the sessions of the real database.
    let tmp = opencode_home();
    let home = fs::canonicalize(tmp.path()).unwrap();
    fs::copy(
        home.join(".local/share/opencode/opencode.db"),
        home.join("zz.db"),
    )
    .unwrap();
    fs::write(
        home.join(".ahrc"),
        "[agents.opencode]\nextra_patterns = [\"~/*.db\"]\n",
    )
    .unwrap();
    ah_opencode(&home)
        .args(["log", "-a", "-o", "agent,id"])
        .assert()
        .success()
        .stdout("opencode\tses_oc1\n")
        .stderr(predicate::str::contains("cannot be told apart"));
}

#[test]
fn broad_extra_pattern_keeps_attributable_files_only() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let other = home.join("other/.claude/projects/-x");
    fs::create_dir_all(&other).unwrap();
    fs::copy(
        fixture_path("claude_session.jsonl"),
        other.join("abc.jsonl"),
    )
    .unwrap();
    fs::copy(
        fixture_path("claude_session.jsonl"),
        home.join("stray.jsonl"),
    )
    .unwrap();
    fs::write(
        home.join(".ahrc"),
        "[agents.claude]\nextra_patterns = [\"~/*/.claude/projects/*/*.jsonl\", \"~/*.jsonl\"]\n",
    )
    .unwrap();
    ah_opencode(&home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(["log", "-a", "-o", "agent,path"])
        .assert()
        .success()
        .stdout(format!(
            "claude\t{}\n",
            other.join("abc.jsonl").to_string_lossy()
        ));
    // `project` would list a stray file as an `unknown` agent if it were
    // collected.
    ah_opencode(&home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(["project", "-o", "agents"])
        .assert()
        .success()
        .stdout("claude\n");
}

#[test]
fn unattributable_extra_match_is_never_opened() {
    let tmp = opencode_home();
    let home = fs::canonicalize(tmp.path()).unwrap();
    fs::write(home.join("notes.db"), "not a database").unwrap();
    fs::write(
        home.join(".ahrc"),
        "[agents.opencode]\nextra_patterns = [\"~/*.db\"]\n",
    )
    .unwrap();
    ah_opencode(&home)
        .args(["log", "-a", "-o", "agent,id"])
        .assert()
        .success()
        .stdout("opencode\tses_oc1\n")
        .stderr(predicate::str::contains("cannot be told apart"))
        .stderr(predicate::str::contains("cannot read").not());
}

#[test]
fn custom_agent_with_home_wide_marker_keeps_default_agent_sessions() {
    // The custom agent's marker (HOME) wins the path attribution, but the
    // built-in patterns are not filtered by owner: output stays as on main.
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let dir = home.join(".codex/sessions/2026/03/24");
    fs::create_dir_all(&dir).unwrap();
    fs::copy(
        fixture_path("codex_session.jsonl"),
        dir.join("rollout-2026-03-24T20-43-12-codex-sess-001.jsonl"),
    )
    .unwrap();
    fs::write(
        home.join(".ahrc"),
        "[agents.wild]\nplugin = \"claude\"\nfile_patterns = [\"~/*/.wild/*.jsonl\"]\n",
    )
    .unwrap();
    ah_opencode(&home)
        .env_remove("CODEX_HOME")
        .args(["log", "-a", "-o", "id"])
        .assert()
        .success()
        .stdout("rollout-2026-03-24T20-43-12-codex-sess-001\n");
}

// ─── Copilot ───────────────────────────────────────────────────────

#[test]
fn copilot_title_time_and_resume() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let state = home.join(".copilot/session-state");
    let named = state.join("cp-named");
    fs::create_dir_all(&named).unwrap();
    fs::copy(
        fixture_path("copilot_workspace.yaml"),
        named.join("workspace.yaml"),
    )
    .unwrap();
    fs::copy(
        fixture_path("copilot_events.jsonl"),
        named.join("events.jsonl"),
    )
    .unwrap();
    // A session that was opened but never got an events.jsonl.
    let empty = state.join("cp-empty");
    fs::create_dir_all(&empty).unwrap();
    fs::write(
        empty.join("workspace.yaml"),
        "id: cp-empty\ncwd: /nonexistent/proj\ncreated_at: 2026-09-27T02:00:00.000Z\n",
    )
    .unwrap();
    // workspace.yaml stops changing early; events.jsonl carries the last activity.
    let at = |rfc3339: &str| -> std::time::SystemTime {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .into()
    };
    let set_mtime = |path: &Path, t| {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap()
    };
    set_mtime(&named.join("workspace.yaml"), at("2026-09-27T02:34:46Z"));
    set_mtime(&named.join("events.jsonl"), at("2026-09-27T03:10:00Z"));
    set_mtime(&empty.join("workspace.yaml"), at("2026-09-27T02:00:05Z"));

    let run = || {
        let mut cmd = ah_opencode(&home);
        cmd.env_remove("COPILOT_HOME").env("TZ", "Asia/Tokyo");
        cmd
    };

    // created_at is UTC in workspace.yaml and shown in local time.
    run()
        .args([
            "log",
            "-a",
            "-o",
            "id,title,created_at,modified_at",
            "-S",
            "id",
            "--asc",
        ])
        .assert()
        .success()
        .stdout(
            "cp-empty\tcp-empty\t2026-09-27 11:00\t2026-09-27 11:00\n\
             cp-named\tFix the 'parser'\t2026-09-27 11:34\t2026-09-27 12:10\n",
        );

    run()
        .args(["log", "-a", "-q", "copilot-needle", "-o", "id"])
        .assert()
        .success()
        .stdout("cp-named\n");

    // Tool-call-only assistant turns (empty content) are not shown.
    run()
        .args(["show", "--json", "cp-named"])
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::contains("please fix the parser"))
        .stdout(predicate::str::contains(r#""role":"assistant""#).count(1));

    run()
        .args(["resume", "--print", "cp-named"])
        .write_stdin("")
        .assert()
        .success()
        .stdout("cd '/nonexistent/proj' && 'copilot' '--resume=cp-named'\n");
}

/// A Gemini session resumed from a legacy `.json` file: Gemini CLI writes the
/// conversation to a sibling `.jsonl` and leaves the `.json` behind.
fn gemini_migrated_session(chats: &Path) -> (String, String) {
    fs::create_dir_all(chats).unwrap();
    let stem = chats.join("session-2026-06-11T20-44-gemmig01");
    let json = format!("{}.json", stem.display());
    let jsonl = format!("{}.jsonl", stem.display());
    fs::write(
        &json,
        r#"{"sessionId":"gemmig01-0000","projectHash":"h","messages":[{"id":"u1","type":"user","content":[{"text":"legacy prompt"}]}]}"#,
    )
    .unwrap();
    fs::write(
        &jsonl,
        concat!(
            r#"{"sessionId":"gemmig01-0000","projectHash":"h","startTime":"2026-06-11T20:44:00.000Z","lastUpdated":"2026-06-11T20:45:00.000Z","kind":"main"}"#,
            "\n",
            r#"{"id":"u1","type":"user","content":[{"text":"legacy prompt"}]}"#,
            "\n",
            r#"{"id":"u2","type":"user","content":[{"text":"resumed prompt"}]}"#,
            "\n",
        ),
    )
    .unwrap();
    (json, jsonl)
}

#[test]
fn gemini_lists_migrated_jsonl_once_with_utc_file_time() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let (_, jsonl) = gemini_migrated_session(&home.join(".gemini/tmp/proj/chats"));
    ah_opencode(&home)
        .env_remove("GEMINI_CLI_HOME")
        .env("TZ", "Asia/Tokyo")
        .args(["log", "-a", "-o", "path,modified_at,id,turns"])
        .assert()
        .success()
        .stdout(format!("{jsonl}\t2026-06-12 05:44\tgemmig01-0000\t2\n"));
    ah_opencode(&home)
        .env_remove("GEMINI_CLI_HOME")
        .args(["show", "-o", "path", "gemmig01"])
        .write_stdin("")
        .assert()
        .success()
        .stdout(format!("{jsonl}\n"));
}

#[test]
fn gemini_custom_agent_globbing_only_json_keeps_legacy_file() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let (json, _) = gemini_migrated_session(&home.join("gemarchive/proj/chats"));
    fs::write(
        home.join(".ahrc"),
        "[agents.gemarch]\nplugin = \"gemini\"\nfile_patterns = [\"~/gemarchive/*/chats/session-*.json\"]\n",
    )
    .unwrap();
    ah_opencode(&home)
        .args(["log", "-a", "-o", "agent,path"])
        .assert()
        .success()
        .stdout(format!("gemarch\t{json}\n"));
}

#[test]
fn gemini_unreadable_jsonl_keeps_legacy_file() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let (json, jsonl) = gemini_migrated_session(&home.join(".gemini/tmp/proj/chats"));
    for broken in [
        "",
        "not json\n",
        // Lines that mention "sessionId" but are not a usable metadata line
        "{\"id\":\"u1\",\"type\":\"user\",\"content\":\"x\",\"sessionId\":\"s\",\"projectHash\":\"h\"}\n",
        "{\"sessionId\":\"\",\"projectHash\":\"h\"}\n",
        "{\"sessionId\":\"s\"}\n",
    ] {
        fs::write(&jsonl, broken).unwrap();
        ah_opencode(&home)
            .env_remove("GEMINI_CLI_HOME")
            .args(["log", "-a", "-o", "path"])
            .assert()
            .success()
            // The unreadable .jsonl has no id and is listed on its own.
            .stdout(predicate::str::contains(format!("{json}\n")));
    }
}

#[test]
fn gemini_disabled_agent_owning_jsonl_keeps_legacy_file() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let (json, jsonl) = gemini_migrated_session(&home.join(".gemini/tmp/proj/chats"));
    fs::write(
        home.join(".ahrc"),
        format!(
            "[agents.off]\nplugin = \"gemini\"\nfile_patterns = [\"{jsonl}\"]\ndisabled = true\n"
        ),
    )
    .unwrap();
    ah_opencode(&home)
        .env_remove("GEMINI_CLI_HOME")
        .args(["log", "-a", "-o", "agent,path"])
        .assert()
        .success()
        // The disabled agent's .jsonl stays hidden.
        .stdout(format!("gemini\t{json}\n"));
}

// ─── Session reference from positional argument vs stdin ─────────────

#[test]
fn positional_session_ignores_piped_stdin() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "-o", "path", &session_path])
        .write_stdin("/nonexistent/other.jsonl\n")
        .assert()
        .success()
        .stdout(format!("{session_path}\n"));
}

#[test]
fn dash_reads_session_from_stdin() {
    let (_tmp, session_path) = codex_session_copy();
    ah().args(["show", "-o", "path", "-"])
        .write_stdin(format!("{session_path}\textra\n"))
        .assert()
        .success()
        .stdout(format!("{session_path}\n"));
}

#[test]
fn dash_with_empty_stdin_fails() {
    ah().args(["resume", "--print", "-"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No session reference on stdin"));
}

#[test]
fn empty_positional_session_is_an_error() {
    for sub in [&["show", "-o", "path"][..], &["resume", "--print"][..]] {
        for empty in ["", "  "] {
            let (_tmp, session_path) = codex_session_copy();
            ah().args(sub)
                .arg(empty)
                .write_stdin(format!("{session_path}\n"))
                .assert()
                .failure()
                .stderr(predicate::str::contains("SESSION is empty"));
        }
    }
}

// Shared-stdin tests need `sh` and the unbuffered Unix stdin reader.
#[cfg(unix)]
#[test]
fn overlong_stdin_line_is_rejected_and_fully_consumed() {
    let (_tmp, session_path) = codex_session_copy();
    let refs = TempDir::new().unwrap();
    let refs_path = refs.path().join("refs.txt");
    // A 70 KiB first line hides a valid path past the 64 KiB limit; the next
    // reader must see the second line, not the tail of the first.
    let long = format!("{}{session_path}", "x".repeat(70 * 1024));
    fs::write(&refs_path, format!("{long}\n{session_path}\n")).unwrap();

    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("\"$0\" show -o path -; echo \"rc=$?\"; \"$0\" show -o path -")
        .arg(assert_cmd::cargo::cargo_bin("ah"))
        .stdin(fs::File::open(&refs_path).unwrap())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout, format!("rc=1\n{session_path}\n"), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("longer than"));
}

#[test]
fn long_trailing_columns_do_not_count_against_the_ref_limit() {
    let (_tmp, session_path) = codex_session_copy();
    let transcript = "t".repeat(200 * 1024);
    ah().args(["show", "-o", "path"])
        .write_stdin(format!("{session_path}\t{transcript}\n"))
        .assert()
        .success()
        .stdout(format!("{session_path}\n"));
}

#[test]
fn invalid_utf8_stdin_is_rejected() {
    ah().args(["resume", "--print", "-"])
        .write_stdin(b"/tmp/rollout-\xff.jsonl\n".to_vec())
        .assert()
        .failure()
        .stderr(predicate::str::contains("not valid UTF-8"));
}

#[cfg(unix)]
#[test]
fn piped_session_ref_consumes_only_the_first_line() {
    let (_tmp1, first) = codex_session_copy();
    let (_tmp2, second) = codex_session_copy();
    let refs = TempDir::new().unwrap();
    let refs_path = refs.path().join("refs.txt");
    fs::write(&refs_path, format!("{first}\n{second}\n")).unwrap();

    // Two readers share one stdin; each must take exactly one line.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("\"$0\" show -o path; \"$0\" show -o path")
        .arg(assert_cmd::cargo::cargo_bin("ah"))
        .stdin(fs::File::open(&refs_path).unwrap())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{first}\n{second}\n")
    );
}

#[test]
fn positional_session_does_not_wait_for_open_stdin() {
    let (_tmp, session_path) = codex_session_copy();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("ah"))
        .args(["show", "-o", "path", &session_path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Keep the write end open: a stdin-first lookup would block here.
    let _stdin = child.stdin.take();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("ah show blocked on an open stdin despite a positional session");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(status.success());
}

fn write_codex_rollout(dir: &Path, stamp: &str, id: &str) -> String {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("rollout-{stamp}-{id}.jsonl"));
    let body = format!(
        "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/tmp/proj\"}}}}\n\
         {{\"type\":\"response_item\",\"payload\":{{\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"hello {id}\"}}]}}}}\n"
    );
    fs::write(&path, body).unwrap();
    path.to_str().unwrap().to_string()
}

fn ah_codex(home: &Path) -> Command {
    let mut cmd = ah_opencode(home);
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd
}

fn set_mtime(path: &str, ago_secs: u64) {
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(ago_secs);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

#[test]
fn codex_archived_sessions_are_listed_and_resumable() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let codex = home.join(".codex");
    let active_id = "019c0000-0000-7000-8000-00000000aaaa";
    let archived_id = "019c0000-0000-7000-8000-00000000bbbb";
    write_codex_rollout(
        &codex.join("sessions/2026/09/27"),
        "2026-09-27T10-00-00",
        active_id,
    );
    let archived = write_codex_rollout(
        &codex.join("archived_sessions"),
        "2026-09-26T10-00-00",
        archived_id,
    );

    ah_codex(&home)
        .args(["log", "-a", "-o", "id,archived", "-S", "id", "--asc"])
        .assert()
        .success()
        .stdout(format!("{active_id}\tfalse\n{archived_id}\ttrue\n"));

    ah_codex(&home)
        .args(["log", "-a", "--no-archived", "-o", "id"])
        .assert()
        .success()
        .stdout(format!("{active_id}\n"));

    // `ah agent` and `ah log` count the same sessions.
    ah_codex(&home)
        .args(["agent", "-a", "--tsv"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("codex\t2\t"));

    // An explicit id reaches an archived session even with --no-archived.
    ah_codex(&home)
        .args(["show", "--no-archived", "-o", "path", &archived_id[..34]])
        .write_stdin("")
        .assert()
        .success()
        .stdout(format!("{archived}\n"));

    ah_codex(&home)
        .args(["resume", "--print", archived_id])
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "'codex' 'resume' '{archived_id}'"
        )));
}

#[test]
fn log_keeps_sessions_without_an_id() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let dir = home.join(".codex/sessions/2026/09/27");
    fs::create_dir_all(&dir).unwrap();
    // No session_meta line and no id in the file name: the session has no id.
    for name in ["a.jsonl", "b.jsonl"] {
        fs::write(
            dir.join(name),
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hi\"}]}}\n",
        )
        .unwrap();
    }
    ah_codex(&home)
        .args(["log", "-a", "-o", "path", "-S", "path", "--asc"])
        .assert()
        .success()
        .stdout(format!(
            "{}\n{}\n",
            dir.join("a.jsonl").display(),
            dir.join("b.jsonl").display()
        ));
}

#[test]
fn duplicate_session_id_resolves_to_newest_copy() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let codex = home.join(".codex");
    let id = "019c0000-0000-7000-8000-00000000cccc";
    let old = write_codex_rollout(
        &codex.join("sessions/2026/09/26"),
        "2026-09-26T10-00-00",
        id,
    );
    let new = write_codex_rollout(
        &codex.join("sessions/2026/09/27"),
        "2026-09-27T10-00-00",
        id,
    );
    set_mtime(&old, 3600);

    for order in ["--asc", "--desc"] {
        ah_codex(&home)
            .args(["log", "-a", "-o", "path", order])
            .assert()
            .success()
            .stdout(format!("{new}\n"));
    }
    // A short id matching two files of one session is not ambiguous.
    ah_codex(&home)
        .args(["show", "-o", "path", &id[..30]])
        .write_stdin("")
        .assert()
        .success()
        .stdout(format!("{new}\n"));
}

#[test]
fn copilot_time_filters_use_events_mtime() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let dir = home.join(".copilot/session-state/0192aaaa-0000-7000-8000-000000000001");
    fs::create_dir_all(&dir).unwrap();
    let yaml = dir.join("workspace.yaml");
    fs::write(
        &yaml,
        "id: 0192aaaa-0000-7000-8000-000000000001\ncwd: /tmp/proj\nname: copilot session\n",
    )
    .unwrap();
    fs::write(
        dir.join("events.jsonl"),
        "{\"type\":\"user.message\",\"data\":{\"content\":\"hello\"}}\n",
    )
    .unwrap();
    set_mtime(yaml.to_str().unwrap(), 10 * 86400);

    ah_opencode(&home)
        .env("COPILOT_HOME", home.join(".copilot"))
        .args(["log", "-a", "--since", "1d", "-o", "title"])
        .assert()
        .success()
        .stdout("copilot session\n");
}

#[test]
fn gemini_session_file_wins_over_newer_logs_json() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let project = home.join(".gemini/tmp/proj");
    let chats = project.join("chats");
    fs::create_dir_all(&chats).unwrap();
    let chat = chats.join("session-2026-06-11T20-44-gemlog01.json");
    fs::write(
        &chat,
        r#"{"sessionId":"gemlog01-0000","messages":[{"type":"user","content":[{"text":"hi"}]}]}"#,
    )
    .unwrap();
    let chat = chat.to_str().unwrap().to_string();
    set_mtime(&chat, 3600);
    // The prompt log is newer but only a secondary record of the session.
    fs::write(
        project.join("logs.json"),
        r#"[{"sessionId":"gemlog01-0000","type":"user","message":"hi"}]"#,
    )
    .unwrap();

    ah_opencode(&home)
        .env_remove("GEMINI_CLI_HOME")
        .args(["log", "-a", "-o", "path"])
        .assert()
        .success()
        .stdout(format!("{chat}\n"));
    ah_opencode(&home)
        .env_remove("GEMINI_CLI_HOME")
        .args(["show", "-o", "path", "gemlog01-0000"])
        .write_stdin("")
        .assert()
        .success()
        .stdout(format!("{chat}\n"));
}

#[test]
fn duplicate_session_id_with_equal_mtime_resolves_consistently() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let codex = home.join(".codex");
    let id = "019c0000-0000-7000-8000-00000000dddd";
    let a = write_codex_rollout(&codex.join("sessions/a"), "2026-09-27T10-00-00", id);
    let z = write_codex_rollout(&codex.join("sessions/z"), "2026-09-27T10-00-00", id);
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for path in [&a, &z] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
    // Ties are broken by path, the same way in log and in id lookups.
    for _ in 0..5 {
        ah_codex(&home)
            .args(["log", "-a", "-o", "path"])
            .assert()
            .success()
            .stdout(format!("{z}\n"));
        ah_codex(&home)
            .args(["show", "-o", "path", id])
            .write_stdin("")
            .assert()
            .success()
            .stdout(format!("{z}\n"));
    }
}

#[test]
fn no_archived_uses_the_attributed_agent() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    // A custom Codex agent whose files live under a Claude-looking path.
    let dir = home.join("logs/.claude/projects/archived_sessions");
    let id = "019c0000-0000-7000-8000-00000000eeee";
    write_codex_rollout(&dir, "2026-09-27T10-00-00", id);
    fs::write(
        home.join(".ahrc"),
        format!(
            "[agents.customcodex]\nplugin = \"codex\"\nfile_patterns = [\"{}/*.jsonl\"]\n",
            dir.display()
        ),
    )
    .unwrap();
    ah_codex(&home)
        .env("CLAUDE_CONFIG_DIR", home.join("logs/.claude"))
        .args(["log", "-a", "-o", "agent,archived"])
        .assert()
        .success()
        .stdout("customcodex\ttrue\n");
    ah_codex(&home)
        .env("CLAUDE_CONFIG_DIR", home.join("logs/.claude"))
        .args(["log", "-a", "--no-archived", "-o", "agent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No sessions found"));
}

#[test]
fn prompt_only_search_matches_escaped_prompts_only() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let dir = home.join(".claude/projects/-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let user = |text: &str| {
        format!(
            r#"{{"type":"user","cwd":"/tmp/proj","message":{{"role":"user","content":"{text}"}}}}"#
        )
    };
    let assistant = |text: &str| {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    };
    let sessions = [
        // the prompt is written verbatim
        ("verbatim", [user("fix the OAuth flow"), assistant("done")]),
        // the prompt is written with `\uXXXX` escapes only
        (
            "escaped",
            [user(r"\u4fee\u6b63 \u004fAuth"), assistant("ok")],
        ),
        // the query appears only outside the prompts
        ("assistant", [user("hello"), assistant("OAuth 修正")]),
    ];
    for (name, lines) in &sessions {
        fs::write(dir.join(format!("{name}.jsonl")), lines.join("\n") + "\n").unwrap();
    }
    let search = |query: &str| {
        let output = ah_opencode(&home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .args(["log", "-a", "-p", "-q", query, "-o", "path"])
            .output()
            .unwrap();
        let mut names: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|p| {
                Path::new(p)
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    };
    assert_eq!(search("oauth"), ["escaped", "verbatim"]);
    assert_eq!(search("修正"), ["escaped"]);
    assert_eq!(search("修正 OAuth"), ["escaped"]);
    assert!(search("done").is_empty());
}

#[cfg(unix)]
#[test]
fn memory_matches_underscore_cwd_dedups_and_lists_shared_agents_md() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let project = home.join("work/my_app");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("AGENTS.md"), "shared rules\n").unwrap();
    // Another path to the same project, even with a different basename, must
    // match through its session's cwd and not list the project twice.
    let link = home.join("alias_app");
    std::os::unix::fs::symlink(&project, &link).unwrap();

    let claude = home.join(".claude");
    let encode = |p: &Path| {
        p.to_string_lossy()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-")
    };
    let encoded_dir = claude.join("projects").join(encode(&project));
    let link_dir = claude.join("projects").join(encode(&link));
    for (dir, cwd) in [(&encoded_dir, &project), (&link_dir, &link)] {
        fs::create_dir_all(dir.join("memory")).unwrap();
        fs::write(
            dir.join("s.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();
    }
    fs::write(
        encoded_dir.join("memory/note.md"),
        "---\nname: \"note\"\ndescription: \"quoted desc\"\ntype: feedback\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        link_dir.join("memory/other.md"),
        "---\nname: other\ndescription: 'it''s'\ntype: project\n---\nbody\n",
    )
    .unwrap();

    let run = |args: &[&str]| {
        let assert = ah()
            .current_dir(&project)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", "/nonexistent")
            .env("GEMINI_CLI_HOME", "/nonexistent")
            .env("COPILOT_HOME", "/nonexistent")
            .env("CURSOR_CONFIG_DIR", "/nonexistent")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .args(args)
            .write_stdin("")
            .assert()
            .success();
        String::from_utf8(assert.get_output().stdout.clone()).unwrap()
    };

    let mut rows: Vec<String> = run(&["memory", "--tsv", "-o", "agent,project,name,description"])
        .lines()
        .map(String::from)
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            "claude\tmy_app\tnote\tquoted desc",
            "claude\tmy_app\tother\tit's",
            "shared\tmy_app\tAGENTS.md\t",
        ]
    );

    let all = run(&["memory", "-a", "--tsv", "-o", "agent,name"]);
    assert_eq!(all.matches("AGENTS.md").count(), 1, "{all}");

    assert_eq!(
        run(&["memory", "--agent", "codex", "--tsv", "-o", "agent,name"]),
        "shared\tAGENTS.md\n"
    );
}

#[cfg(unix)]
#[test]
fn memory_keeps_project_attribution_for_symlinked_files() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let claude = home.join(".claude");
    fs::create_dir_all(&claude).unwrap();
    let a = home.join("work/project_a");
    let b = home.join("work/project_b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    // Project CLAUDE.md is a symlink to the global one.
    fs::write(claude.join("CLAUDE.md"), "global rules\n").unwrap();
    std::os::unix::fs::symlink(claude.join("CLAUDE.md"), a.join("CLAUDE.md")).unwrap();
    // Two projects share one real AGENTS.md.
    fs::write(home.join("AGENTS.md"), "shared rules\n").unwrap();
    std::os::unix::fs::symlink(home.join("AGENTS.md"), a.join("AGENTS.md")).unwrap();
    std::os::unix::fs::symlink(home.join("AGENTS.md"), b.join("AGENTS.md")).unwrap();
    // Sessions so that -a discovers both projects.
    for (i, p) in [&a, &b].into_iter().enumerate() {
        let dir = claude.join(format!("projects/p{i}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("s.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n",
                p.display()
            ),
        )
        .unwrap();
    }

    let run = |dir: &Path, args: &[&str]| {
        let assert = ah()
            .current_dir(dir)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", "/nonexistent")
            .env("GEMINI_CLI_HOME", "/nonexistent")
            .env("COPILOT_HOME", "/nonexistent")
            .env("CURSOR_CONFIG_DIR", "/nonexistent")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .args(args)
            .write_stdin("")
            .assert()
            .success();
        String::from_utf8(assert.get_output().stdout.clone()).unwrap()
    };

    let mut rows: Vec<String> = run(&a, &["memory", "--tsv", "-o", "agent,project,name"])
        .lines()
        .map(String::from)
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            "claude\tproject_a\tCLAUDE.md",
            "shared\tproject_a\tAGENTS.md"
        ]
    );
    // Stable across runs: each project keeps its own AGENTS.md row.
    for _ in 0..5 {
        assert_eq!(
            run(
                &a,
                &[
                    "memory",
                    "-a",
                    "--project",
                    "project_b",
                    "--tsv",
                    "-o",
                    "name"
                ]
            ),
            "AGENTS.md\n"
        );
    }
}

#[test]
fn memory_project_cwd_skips_sessions_without_cwd() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    // Long enough for Claude Code to truncate the encoded name and add a hash.
    let project = home.join(format!("work/{}/my_app", "d".repeat(220)));
    fs::create_dir_all(&project).unwrap();
    let claude = home.join(".claude");
    let encoded: String = project
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = claude
        .join("projects")
        .join(format!("{}-1a2b3c", &encoded[..200]));
    fs::create_dir_all(dir.join("memory")).unwrap();
    fs::write(
        dir.join("old.jsonl"),
        format!(
            "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n",
            project.display()
        ),
    )
    .unwrap();
    // A newer session without a cwd (e.g. still being written).
    fs::write(dir.join("new.jsonl"), "{\"type\":\"summary\"}\n").unwrap();
    let old = fs::File::open(dir.join("old.jsonl")).unwrap();
    old.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
        .unwrap();
    fs::write(
        dir.join("memory/note.md"),
        "---\nname: note\ntype: feedback\n---\nbody\n",
    )
    .unwrap();

    ah().current_dir(&project)
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &claude)
        .env("CODEX_HOME", "/nonexistent")
        .env("GEMINI_CLI_HOME", "/nonexistent")
        .env("COPILOT_HOME", "/nonexistent")
        .env("CURSOR_CONFIG_DIR", "/nonexistent")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .args(["memory", "--tsv", "-o", "project,name"])
        .write_stdin("")
        .assert()
        .success()
        .stdout("my_app\tnote\n");
}

#[test]
fn memory_lists_each_agents_instruction_rule_and_memory_files() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let write = |rel: &str, content: &str| {
        let path = home.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    // Global files
    write(".claude/CLAUDE.md", "claude global\n");
    write(
        ".claude/rules/style.md",
        "---\ndescription: house style\n---\nuse tabs\n",
    );
    write(
        ".claude/agent-memory/reviewer/notes.md",
        "---\nname: review-notes\n---\nnotes\n",
    );
    write(
        ".claude/skills/s1/SKILL.md",
        "---\nname: s1\ndescription: skill one\n---\nbody\n",
    );
    write(".codex/AGENTS.md", "codex global\n");
    write(".codex/memories/m1.md", "remember this\n");
    write(
        ".gemini/settings.json",
        r#"{"context":{"fileName":["AGENTS.md","CONTEXT.md"]}}"#,
    );
    write(".gemini/CONTEXT.md", "gemini context\n");
    write(".gemini/GEMINI.md", "not loaded with a custom fileName\n");
    write(".gemini/config/rules/r.md", "agy rule\n");
    write(".grok/AGENTS.md", "grok global\n");
    write(".grok/memory-v2/global/topics/g.md", "# G\nglobal topic\n");
    write(".grok/memory-v2/global/MEMORY.md", "# index\n");
    write(
        ".grok/memory-v2/workspaces/repo-0123abcd/topics/t.md",
        "# T\nworkspace topic\n",
    );
    write(
        ".grok/memory-v2/workspaces/other-89abcdef/topics/o.md",
        "# O\nother workspace\n",
    );
    write(".copilot/copilot-instructions.md", "copilot global\n");
    write(
        ".cursor/rules/u.mdc",
        "---\ndescription: user rule\n---\nx\n",
    );
    write(
        ".config/opencode/opencode.jsonc",
        "{\n  // comment\n  \"instructions\": [\"~/extra.md\",],\n}\n",
    );
    write("extra.md", "opencode extra\n");
    write(
        ".agents/skills/sk/SKILL.md",
        "---\nname: sk\n---\nshared skill\n",
    );
    // Project files at the repository root; the command runs in a subdirectory.
    write("repo/.git/HEAD", "ref: refs/heads/main\n");
    write("repo/CLAUDE.local.md", "local\n");
    write("repo/.claude/rules/x.md", "project rule\n");
    write("repo/.cursorrules", "cursor legacy\n");
    write("repo/.cursor/rules/p.mdc", "project cursor rule\n");
    write("repo/.github/copilot-instructions.md", "copilot project\n");
    write("repo/.agents/rules/a.md", "agy project rule\n");
    write("repo/AGENTS.override.md", "codex override\n");
    write("repo/AGENTS.md", "shared\n");
    write("repo/CONTEXT.md", "gemini project\n");
    write("repo/opencode.json", r#"{"instructions":["docs/*.md"]}"#);
    write("repo/docs/d.md", "opencode project doc\n");
    write("repo/.grok/rules/gr.md", "grok project rule\n");
    write(".grok/rules/gg.md", "grok global rule\n");
    write(".grok/memory/MEMORY.md", "- legacy note\n");
    write(".cursor/rules/team/nested.mdc", "nested user rule\n");
    // An intermediate directory between the repository root and the cwd.
    write("repo/pkg/AGENTS.md", "package rules\n");
    let cwd = home.join("repo/pkg/sub");
    fs::create_dir_all(&cwd).unwrap();

    let run = |args: &[&str]| {
        let mut cmd = ah();
        cmd.current_dir(&cwd).env("HOME", &home);
        for var in [
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "GEMINI_CLI_HOME",
            "COPILOT_HOME",
            "CURSOR_DATA_DIR",
            "GROK_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
        ] {
            cmd.env_remove(var);
        }
        let assert = cmd.args(args).write_stdin("").assert().success();
        let mut rows: Vec<String> = String::from_utf8(assert.get_output().stdout.clone())
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        rows.sort();
        rows
    };

    assert_eq!(
        run(&[
            "memory",
            "--tsv",
            "-o",
            "agent,project,type,name,description"
        ]),
        vec![
            "agy\t(global)\trule\tr.md\t",
            "agy\trepo\trule\ta.md\t",
            "claude\t(global)\tinstruction\tCLAUDE.md\t",
            "claude\t(global)\tmemory\treview-notes\t",
            "claude\t(global)\trule\tstyle.md\thouse style",
            "claude\trepo\tinstruction\tCLAUDE.local.md\t",
            "claude\trepo\trule\tx.md\t",
            "codex\t(global)\tinstruction\tAGENTS.md\t",
            "codex\t(global)\tmemory\tm1\t",
            "codex\trepo\tinstruction\tAGENTS.override.md\t",
            "copilot\t(global)\tinstruction\tcopilot-instructions.md\t",
            "copilot\trepo\tinstruction\tcopilot-instructions.md\t",
            "cursor\t(global)\trule\tnested.mdc\t",
            "cursor\t(global)\trule\tu.mdc\tuser rule",
            "cursor\trepo\tinstruction\t.cursorrules\t",
            "cursor\trepo\trule\tp.mdc\t",
            "gemini\t(global)\tinstruction\tCONTEXT.md\t",
            "gemini\trepo\tinstruction\tCONTEXT.md\t",
            "grok\t(global)\tinstruction\tAGENTS.md\t",
            "grok\t(global)\tmemory\tMEMORY\t",
            "grok\t(global)\tmemory\tg\t",
            "grok\t(global)\trule\tgg.md\t",
            "grok\trepo\tmemory\tt\t",
            "grok\trepo\trule\tgr.md\t",
            "opencode\t(global)\tinstruction\textra.md\t",
            "opencode\trepo\tinstruction\td.md\t",
            "shared\tpkg\tinstruction\tAGENTS.md\t",
            "shared\trepo\tinstruction\tAGENTS.md\t",
        ]
    );

    assert_eq!(
        run(&[
            "memory",
            "-t",
            "skill",
            "--tsv",
            "-o",
            "agent,name,description"
        ]),
        vec!["claude\ts1\tskill one", "shared\tsk\t"]
    );
}

/// `ah memory` with only memory-related env vars cleared, run in `cwd`.
fn memory_cmd(home: &Path, cwd: &Path) -> Command {
    let mut cmd = ah();
    cmd.current_dir(cwd).env("HOME", home);
    for var in [
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "COPILOT_HOME",
        "CURSOR_DATA_DIR",
        "GROK_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

#[test]
fn memory_all_scans_repository_root_of_known_cwds() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let sub = home.join("repo/sub");
    fs::create_dir_all(&sub).unwrap();
    fs::create_dir_all(home.join("repo/.git")).unwrap();
    fs::write(home.join("repo/AGENTS.md"), "root rules\n").unwrap();
    // The only known session was started in the subdirectory.
    let project_dir = home.join(".claude/projects/-sub");
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(
        project_dir.join("s.jsonl"),
        format!(
            "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n",
            sub.display()
        ),
    )
    .unwrap();

    memory_cmd(&home, &home)
        .args(["memory", "-a", "--tsv", "-o", "agent,project,name"])
        .write_stdin("")
        .assert()
        .success()
        .stdout("shared\trepo\tAGENTS.md\n");
}

#[cfg(unix)]
#[test]
fn memory_keeps_global_files_global_through_a_home_alias() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude/CLAUDE.md"), "global\n").unwrap();
    let alias = home.join("alias");
    std::os::unix::fs::symlink(&home, &alias).unwrap();

    memory_cmd(&home, &home)
        .env("CLAUDE_CONFIG_DIR", alias.join(".claude"))
        .args(["memory", "--tsv", "-o", "agent,project,name"])
        .write_stdin("")
        .assert()
        .success()
        .stdout("claude\t(global)\tCLAUDE.md\n");
}

#[cfg(unix)]
#[test]
fn memory_recursive_patterns_survive_symlink_cycles() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let rules = home.join(".claude/rules");
    fs::create_dir_all(rules.join("nested")).unwrap();
    fs::write(rules.join("nested/r.md"), "rule\n").unwrap();
    std::os::unix::fs::symlink(".", rules.join("l1")).unwrap();
    std::os::unix::fs::symlink(".", rules.join("l2")).unwrap();
    std::os::unix::fs::symlink("..", rules.join("nested/up")).unwrap();

    memory_cmd(&home, &home)
        .timeout(std::time::Duration::from_secs(20))
        .args(["memory", "--tsv", "-o", "agent,type,name"])
        .write_stdin("")
        .assert()
        .success()
        .stdout("claude\trule\tr.md\n");
}

#[cfg(unix)]
#[test]
fn memory_symlinked_project_directory_keeps_project_attribution() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    fs::create_dir_all(home.join(".claude/rules")).unwrap();
    fs::write(home.join(".claude/rules/shared.md"), "shared rule\n").unwrap();
    let repo = home.join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    std::os::unix::fs::symlink(home.join(".claude/rules"), repo.join(".claude/rules")).unwrap();

    let assert = memory_cmd(&home, &repo)
        .args(["memory", "--tsv", "-o", "agent,project,name"])
        .write_stdin("")
        .assert()
        .success();
    let mut rows: Vec<String> = String::from_utf8(assert.get_output().stdout.clone())
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    rows.sort();
    // The same file is listed once, attributed to the project (#35).
    assert_eq!(rows, vec!["claude\trepo\tshared.md"]);
}

#[cfg(unix)]
#[test]
fn memory_dot_components_do_not_bypass_cycle_protection() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let rules = home.join(".claude/rules");
    fs::create_dir_all(&rules).unwrap();
    fs::write(rules.join("r.md"), "rule\n").unwrap();
    std::os::unix::fs::symlink(".", rules.join("l1")).unwrap();
    std::os::unix::fs::symlink(".", rules.join("l2")).unwrap();

    memory_cmd(&home, &home)
        .env("CLAUDE_CONFIG_DIR", home.join(".").join(".claude"))
        .timeout(std::time::Duration::from_secs(20))
        .args(["memory", "--tsv", "-o", "agent,type,name"])
        .write_stdin("")
        .assert()
        .success()
        .stdout("claude\trule\tr.md\n");
}

#[test]
fn codex_subagent_sessions_are_hidden_unless_requested() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let dir = home.join(".codex/sessions/2026/09/27");
    let parent_id = "019c0000-0000-7000-8000-00000000aaaa";
    let child_id = "019c0000-0000-7000-8000-00000000cccc";
    write_codex_rollout(&dir, "2026-09-27T10-00-00", parent_id);
    fs::write(
        dir.join(format!("rollout-2026-09-27T10-05-00-{child_id}.jsonl")),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{child_id}\",\"cwd\":\"/tmp/proj\",\
             \"source\":{{\"subagent\":{{\"thread_spawn\":{{\"parent_thread_id\":\"{parent_id}\",\"depth\":1}}}}}}}}}}\n"
        ),
    )
    .unwrap();

    ah_codex(&home)
        .args(["log", "-a", "-o", "id"])
        .assert()
        .success()
        .stdout(format!("{parent_id}\n"));
    ah_codex(&home)
        .args(["agent", "-a", "--tsv"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("codex\t1\t"));
    ah_codex(&home)
        .args([
            "log",
            "-a",
            "--subagents",
            "-o",
            "id,parent_id",
            "-S",
            "id",
            "--asc",
        ])
        .assert()
        .success()
        .stdout(format!("{parent_id}\t\n{child_id}\t{parent_id}\n"));
    // Spawned Codex threads stay resumable by id.
    ah_codex(&home)
        .args(["resume", "--print", child_id])
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "'codex' 'resume' '{child_id}'"
        )));
}

#[test]
fn claude_subagent_transcripts_are_listed_with_subagents() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let claude = home.join(".claude");
    let project = claude.join("projects/-tmp-proj");
    let parent_id = "11111111-2222-3333-4444-555555555555";
    let line = |text: &str| {
        format!(
            "{{\"type\":\"user\",\"cwd\":\"/tmp/proj\",\"sessionId\":\"{parent_id}\",\
             \"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
    };
    fs::create_dir_all(project.join(parent_id).join("subagents")).unwrap();
    fs::write(project.join(format!("{parent_id}.jsonl")), line("parent")).unwrap();
    let child = project.join(format!("{parent_id}/subagents/agent-a1b2c3.jsonl"));
    fs::write(&child, line("child")).unwrap();
    let ah_claude = || {
        let mut cmd = ah_opencode(&home);
        cmd.env("CLAUDE_CONFIG_DIR", &claude);
        cmd
    };

    ah_claude()
        .args(["log", "-a", "-o", "id"])
        .assert()
        .success()
        .stdout(format!("{parent_id}\n"));
    ah_claude()
        .args([
            "log",
            "-a",
            "--subagents",
            "-o",
            "id,parent_id,project",
            "-S",
            "id",
            "--asc",
        ])
        .assert()
        .success()
        .stdout(format!("{parent_id}\t\tproj\na1b2c3\t{parent_id}\tproj\n"));
    // The agent id reaches the transcript without --subagents, but a
    // subagent cannot be resumed on its own.
    ah_claude()
        .args(["show", "-o", "path", "a1b2c3"])
        .write_stdin("")
        .assert()
        .success()
        .stdout(format!("{}\n", child.display()));
    ah_claude()
        .args(["resume", "--print", "a1b2c3"])
        .write_stdin("")
        .assert()
        .failure();
}

#[test]
fn grok_and_cursor_subagents_are_hidden_by_default() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let grok = home.join(".grok");
    for (id, kind) in [("0192-main", "interactive"), ("0192-sub", "subagent")] {
        let dir = grok.join("sessions/%2Ftmp%2Fproj").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("chat_history.jsonl"), "").unwrap();
        fs::write(
            dir.join("summary.json"),
            format!(
                "{{\"info\":{{\"id\":\"{id}\",\"cwd\":\"/tmp/proj\"}},\"session_kind\":\"{kind}\"}}"
            ),
        )
        .unwrap();
    }
    let cursor = home.join(".cursor");
    let transcripts = cursor.join("projects/tmp-proj/agent-transcripts");
    fs::create_dir_all(transcripts.join("chat-1/subagents")).unwrap();
    let message =
        "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n";
    fs::write(transcripts.join("chat-1/chat-1.jsonl"), message).unwrap();
    fs::write(transcripts.join("chat-1/subagents/sub-1.jsonl"), message).unwrap();
    let ah_both = || {
        let mut cmd = ah_opencode(&home);
        cmd.env("GROK_HOME", &grok).env("CURSOR_DATA_DIR", &cursor);
        cmd
    };

    ah_both()
        .args(["log", "-a", "-o", "agent,id", "-S", "id", "--asc"])
        .assert()
        .success()
        .stdout("grok\t0192-main\ncursor\tchat-1\n");
    ah_both()
        .args([
            "log",
            "-a",
            "--subagents",
            "-o",
            "agent,id,parent_id,resume_cmd",
            "-S",
            "id",
            "--asc",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("grok\t0192-sub\t\t"))
        .stdout(predicate::str::contains("cursor\tsub-1\tchat-1\t\n"));
}

#[test]
fn claude_subagents_sharing_an_id_under_different_parents_stay_distinct() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let claude = home.join(".claude");
    let project = claude.join("projects/-tmp-proj");
    let mut children = Vec::new();
    for parent in ["parent-a", "parent-b"] {
        fs::create_dir_all(project.join(parent).join("subagents")).unwrap();
        fs::write(
            project.join(format!("{parent}.jsonl")),
            "{\"type\":\"user\",\"cwd\":\"/tmp/proj\",\"message\":{\"role\":\"user\",\"content\":\"p\"}}\n",
        )
        .unwrap();
        let child = project.join(format!("{parent}/subagents/agent-dup1.jsonl"));
        fs::write(
            &child,
            "{\"type\":\"user\",\"cwd\":\"/tmp/proj\",\"message\":{\"role\":\"user\",\"content\":\"c\"}}\n",
        )
        .unwrap();
        children.push(child);
    }
    let ah_claude = || {
        let mut cmd = ah_opencode(&home);
        cmd.env("CLAUDE_CONFIG_DIR", &claude);
        cmd
    };

    ah_claude()
        .args([
            "log",
            "-a",
            "--subagents",
            "-o",
            "id,parent_id",
            "-S",
            "parent_id",
            "--asc",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("dup1\tparent-a\ndup1\tparent-b\n"));
    ah_claude()
        .args(["show", "-o", "path", "dup1"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Ambiguous session id"));
    // The path still selects one of them.
    ah_claude()
        .args(["show", "-o", "parent_id", children[1].to_str().unwrap()])
        .write_stdin("")
        .assert()
        .success()
        .stdout("parent-b\n");
}

#[test]
fn gemini_project_named_chats_is_not_a_subagent() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let gemini = home.join(".gemini");
    let chats = gemini.join("tmp/chats/chats");
    fs::create_dir_all(&chats).unwrap();
    fs::write(
        chats.join("session-2026-06-11T02-44-aaaa1111.jsonl"),
        "{\"sessionId\":\"aaaa1111-0000\",\"projectHash\":\"h\",\"kind\":\"main\"}\n",
    )
    .unwrap();
    let mut cmd = ah_opencode(&home);
    cmd.env("GEMINI_CLI_HOME", &gemini)
        .args(["log", "-a", "-o", "id,parent_id"])
        .assert()
        .success()
        .stdout("aaaa1111-0000\t\n");
}

#[test]
fn opencode_child_sessions_are_listed_only_with_subagents() {
    let tmp = opencode_home();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let child_dir = home.join("child-only");
    fs::create_dir_all(&child_dir).unwrap();
    fs::write(child_dir.join("AGENTS.md"), "child rules\n").unwrap();
    let conn = rusqlite::Connection::open(home.join(".local/share/opencode/opencode.db")).unwrap();
    conn.execute(
        "INSERT INTO session VALUES ('ses_child', 'ses_oc1', ?1, 'Child', 1700000000000, 1700000200000)",
        [child_dir.to_str().unwrap()],
    )
    .unwrap();
    drop(conn);

    ah_opencode(&home)
        .args(["log", "-a", "-o", "id"])
        .assert()
        .success()
        .stdout("ses_oc1\n");
    ah_opencode(&home)
        .args(["log", "-a", "--subagents", "-o", "id,parent_id"])
        .assert()
        .success()
        .stdout("ses_child\tses_oc1\nses_oc1\t\n");
    // Project discovery for `ah memory` still ignores child sessions.
    ah_opencode(&home)
        .args(["memory", "-a", "-o", "path"])
        .assert()
        .stdout(predicate::str::contains("child-only").not());
}

#[test]
fn default_search_covers_tool_io_and_raw_search_covers_metadata() {
    let tmp = TempDir::new().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let dir = home.join(".claude/projects/-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let lines = [
        r#"{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"run the tests"}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_meta_only","name":"Bash","input":{"command":"cargo test arg-needle"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_meta_only","content":"out-needle: ok"}]}}"#,
    ];
    fs::write(dir.join("s1.jsonl"), lines.join("\n") + "\n").unwrap();
    let hits = |args: &[&str]| {
        let output = ah_opencode(&home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .args(["log", "-a", "-o", "id"])
            .args(args)
            .output()
            .unwrap();
        !String::from_utf8(output.stdout).unwrap().trim().is_empty()
    };
    // default: messages, tool arguments and tool output, not metadata
    assert!(hits(&["-q", "run the tests"]));
    assert!(hits(&["-q", "arg-needle"]));
    assert!(hits(&["-q", "out-needle"]));
    assert!(!hits(&["-q", "toolu_meta_only"]));
    assert!(!hits(&["-q", "tool_use_id"]));
    // --raw-search: the raw file, metadata included
    assert!(hits(&["--raw-search", "-q", "toolu_meta_only"]));
    assert!(hits(&["--raw-search", "-q", "tool_use_id"]));
    // -p: user prompts only
    assert!(hits(&["-p", "-q", "run the tests"]));
    assert!(!hits(&["-p", "-q", "out-needle"]));
    // -p and --raw-search conflict
    ah_opencode(&home)
        .args(["log", "-a", "-p", "--raw-search", "-q", "x"])
        .assert()
        .failure();
}

#[test]
fn occurrence_search_reads_opencode_virtual_sessions() {
    let tmp = opencode_home();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let output = ah_opencode(&home)
        .args(["search", "parser", "-a", "--json", "--max-matches", "0"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hits: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert!(!hits.is_empty());
    assert!(
        hits.iter()
            .all(|h| h["agent"] == "opencode" && h["id"] == "ses_oc1")
    );
}

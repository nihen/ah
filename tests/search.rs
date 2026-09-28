use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn test_tempdir() -> TempDir {
    // macOS exposes /var through /private/var; cwd uses the physical path.
    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

fn ah(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ah").unwrap();
    cmd.env("HOME", home).current_dir(home);
    for var in [
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "COPILOT_HOME",
        "CURSOR_DATA_DIR",
        "GROK_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "NO_COLOR",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn write_session(home: &Path, id: &str, cwd: &Path, texts: &[Value]) -> String {
    let path = home.join(format!(".claude/projects/test/{id}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut lines = vec![
        json!({"type":"user", "sessionId":id, "cwd":cwd,
        "message":{"role":"user", "content":"start"}})
        .to_string(),
    ];
    lines.extend(texts.iter().map(Value::to_string));
    fs::write(&path, lines.join("\n")).unwrap();
    path.to_str().unwrap().to_owned()
}

fn fixture() -> TempDir {
    let tmp = test_tempdir();
    write_session(
        tmp.path(),
        "one",
        tmp.path(),
        &[
            json!({"type":"user","message":{"role":"user","content":"認証 needle twice NEEDLE"}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"text","text":"answer needle"},
                {"type":"text","text":"second block needle"},
                {"type":"tool_use","id":"metadata-secret","name":"Bash","input":{"command":"echo needle"}}
            ]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":"output needle"}]}}),
            json!({"type":"system","content":"metadata-secret"}),
        ],
    );
    tmp
}

fn search(home: &Path, args: &[&str]) -> Vec<Value> {
    let out = ah(home)
        .arg("search")
        .args(args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn search_enumerates_all_occurrences_but_log_still_lists_sessions() {
    let tmp = fixture();
    let hits = search(tmp.path(), &["needle"]);
    assert_eq!(hits.len(), 6);
    assert!(hits.iter().all(|h| h["id"] == "one"));
    assert_eq!(hits[0]["text_index"], hits[1]["text_index"]);
    assert_eq!(hits[0]["match_start"], "認証 ".len());
    assert_eq!(hits[1]["match_start"], "認証 needle twice ".len());
    assert!(hits[4]["snippet"].as_str().unwrap().contains("echo needle"));
    assert!(
        hits[5]["snippet"]
            .as_str()
            .unwrap()
            .contains("output needle")
    );
    let out = ah(tmp.path())
        .args(["log", "-q", "needle", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
    assert_eq!(hits, search(tmp.path(), &["-q", "needle"]));
    assert_eq!(hits, {
        let out = ah(tmp.path())
            .args(["-q", "needle", "search", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect::<Vec<Value>>()
    });
}

#[test]
fn search_prompt_only_and_metadata_exclusion() {
    let tmp = fixture();
    assert_eq!(search(tmp.path(), &["needle", "-p"]).len(), 2);
    assert!(search(tmp.path(), &["metadata-secret"]).is_empty());
    assert!(search(tmp.path(), &["missing"]).is_empty());
    assert_eq!(search(tmp.path(), &["(?-i)NEEDLE"]).len(), 1);
}

#[test]
fn search_limits_occurrences_and_snippet_size() {
    let tmp = fixture();
    assert_eq!(
        search(tmp.path(), &["needle", "--max-matches", "2"]).len(),
        2
    );
    assert_eq!(
        search(tmp.path(), &["needle", "--max-matches", "0"]).len(),
        6
    );
    let long = format!("認証{}", "あ".repeat(5000));
    write_session(
        tmp.path(),
        "long",
        tmp.path(),
        &[json!({"type":"assistant","message":{"content":long}})],
    );
    let hits = search(tmp.path(), &["認証あ+", "--snippet-length", "12"]);
    assert_eq!(hits.len(), 1);
    assert!(hits[0]["snippet"].as_str().unwrap().chars().count() <= 12);
    assert_eq!(hits[0]["match_end"], long.len());
}

#[test]
fn search_zero_width_is_bounded_and_skips_empty_fragments() {
    let tmp = test_tempdir();
    write_session(
        tmp.path(),
        "one",
        tmp.path(),
        &[json!({"type":"assistant","message":{"content":[
            {"type":"text","text":""}, {"type":"text","text":"あいう"}
        ]}})],
    );
    assert!(search(tmp.path(), &["^$"]).is_empty());
    let hits = search(tmp.path(), &["^", "--max-matches", "0"]);
    assert_eq!(hits.len(), 2); // start, あいう
    assert!(
        hits.iter()
            .all(|h| h["match_start"] == 0 && h["match_end"] == 0)
    );
}

#[test]
fn search_filters_and_session_order() {
    let tmp = fixture();
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let path = write_session(
        tmp.path(),
        "older",
        &elsewhere,
        &[json!({"type":"user","message":{"content":"needle older"}})],
    );
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    assert_eq!(search(tmp.path(), &["needle"]).len(), 6);
    let hits = search(tmp.path(), &["needle", "-a"]);
    assert_eq!(hits.len(), 7);
    assert_eq!(hits.last().unwrap()["id"], "older");
    assert_eq!(
        search(tmp.path(), &["needle", "-d", elsewhere.to_str().unwrap()]).len(),
        1
    );
    assert_eq!(
        search(tmp.path(), &["needle", "-a", "--since", "30m"]).len(),
        7
    ); // m = months, not minutes
    assert_eq!(
        search(tmp.path(), &["needle", "-a", "--agent", "codex"]).len(),
        0
    );
    assert_eq!(search(tmp.path(), &["needle", "-a", "-n", "1"]).len(), 6);
    assert_eq!(
        search(tmp.path(), &["needle", "-a", "--max-matches", "1"])[0]["id"],
        "one"
    );
}

#[test]
fn search_rejects_invalid_and_unsupported_options() {
    let tmp = test_tempdir();
    for args in [
        vec![],
        vec![""],
        vec!["["],
        vec!["x", "-q", "y"],
        vec!["x", "--raw-search"],
        vec!["x", "-i", "--json"],
        vec!["x", "--snippet-length", "0"],
        vec!["x", "--json", "--tsv"],
        vec!["x", "--remote", "missing"],
    ] {
        ah(tmp.path()).arg("search").args(args).assert().failure();
    }
    ah(tmp.path())
        .args(["search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Show matching passages"));
    ah(tmp.path())
        .args(["man", "search"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ah-search"));
    ah(tmp.path())
        .args(["completion", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("search"));
}

#[test]
fn search_tsv_escapes_multiline_text_and_backslashes() {
    let tmp = test_tempdir();
    write_session(
        tmp.path(),
        "one",
        tmp.path(),
        &[json!({"type":"user","message":{"content":"needle\nnext\twith\\literal"}})],
    );
    let output = ah(tmp.path())
        .args(["search", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    let rows: Vec<_> = output.lines().collect();
    assert_eq!(rows.len(), 1);
    let cols: Vec<_> = rows[0].split('\t').collect();
    assert_eq!(cols.len(), 10);
    assert_eq!(cols[8], "user");
    assert!(cols[9].starts_with("v1:"));
    assert_eq!(cols[7], "needle\\nnext\\twith\\\\literal");
    assert!(Path::new(cols[0]).is_file());
}

#[test]
fn search_empty_home_is_success() {
    let tmp = test_tempdir();
    assert!(search(tmp.path(), &["needle", "-a"]).is_empty());
}

#[cfg(unix)]
#[test]
fn search_remote_runs_on_host_and_merges_with_global_limit() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = fixture();
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        tmp.path().join(".ahrc"),
        "[remotes.test]\nhost = 'example.test'\nah_path = 'remote-ah'\n",
    )
    .unwrap();
    let hit = json!({"path":"/remote/one.jsonl", "agent":"claude", "project":"remote",
        "id":"one", "title":"remote session", "modified_at":"2099-01-01 00:00",
        "text_index":2, "match_start":0, "match_end":6, "snippet":"needle", "kind":"user", "position":format!("v1:2:1:0:6:{}", "a".repeat(64)), "_snippet_match":[0,6]});
    let script = bin.join("ssh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/ssh-args\"\ncat <<'RESULT'\n{hit}\nRESULT\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = ah(tmp.path())
        .env("PATH", &path)
        .args(["search", "needle", "-A", "--max-matches", "1", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hit: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(hit["path"], "test:/remote/one.jsonl");
    let ssh_args = fs::read_to_string(tmp.path().join("ssh-args")).unwrap();
    assert!(ssh_args.contains("'remote-ah' 'search' '--json' '-a' '-q' 'needle'"));
    assert!(ssh_args.contains("'--max-matches' '1'"));
    assert!(ssh_args.contains("'--search-wire'"));
    assert!(hit.get("_snippet_match").is_none());
    let terminal = terminal_search(
        tmp.path(),
        &["needle", "--remote", "test", "--max-matches", "1"],
        &[("PATH", &path)],
    );
    assert!(terminal.contains("[claude] remote  remote:test"));
    assert!(!terminal.contains('·'));
    assert!(!terminal.contains("test:/remote/one.jsonl"));
    let verbose = terminal_search(
        tmp.path(),
        &[
            "needle",
            "--remote",
            "test",
            "--max-matches",
            "1",
            "--verbose",
        ],
        &[("PATH", &path)],
    );
    assert!(verbose.contains("test:/remote/one.jsonl"));
    assert!(verbose.contains("text #2  bytes 0..6"));
    assert!(terminal.contains("\x1b[1;33mneedle\x1b[0m"));
    assert!(!ssh_args.contains("'--remote'"));
    assert!(!ssh_args.contains("'-A'"));
    // Search succeeds on empty results; every failed SSH command is an error,
    // even if stderr includes an older command's empty-result message.
    fs::write(&script, "#!/bin/sh\necho 'No sessions found' >&2\nexit 1\n").unwrap();
    ah(tmp.path())
        .env("PATH", &path)
        .args(["search", "needle", "--remote", "test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No sessions found"));
    // A remote running an old version must not be silently treated as zero matches.
    fs::write(&script, "#!/bin/sh\necho unsupported >&2\nexit 2\n").unwrap();
    ah(tmp.path())
        .env("PATH", &path)
        .args(["search", "needle", "--remote", "test"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("unsupported"));
    fs::write(&script, "#!/bin/sh\necho '{\"id\":\"old log schema\"}'\n").unwrap();
    ah(tmp.path())
        .env("PATH", &path)
        .args(["search", "needle", "--remote", "test"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("Update ah"));
}

#[test]
fn search_order_is_deterministic_at_the_limit_for_equal_session_dates() {
    let tmp = test_tempdir();
    let mut paths = Vec::new();
    for id in ["z", "a", "b"] {
        paths.push(write_session(
            tmp.path(),
            id,
            tmp.path(),
            &[json!({"type":"user", "message":{"content":"needle needle"}})],
        ));
    }
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(86400);
    for path in paths {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
    let all = search(tmp.path(), &["needle", "--max-matches", "0"]);
    assert_eq!(all.len(), 6);
    assert_eq!(all[0]["id"], "a");
    assert_eq!(all[2]["id"], "b");
    assert_eq!(all[4]["id"], "z");
    assert_eq!(
        search(tmp.path(), &["needle", "--max-matches", "3"]),
        all[..3]
    );
    assert!(search(tmp.path(), &["needle", "--since", "1h"]).is_empty());
    assert_eq!(search(tmp.path(), &["needle", "--until", "1h"]).len(), 6);
}

#[test]
fn search_default_cap_is_one_hundred_occurrences() {
    let tmp = test_tempdir();
    write_session(
        tmp.path(),
        "many",
        tmp.path(),
        &[json!({"type":"user", "message":{"content":"needle ".repeat(120)}})],
    );
    assert_eq!(search(tmp.path(), &["needle"]).len(), 100);
    assert_eq!(
        search(tmp.path(), &["needle", "--max-matches", "0"]).len(),
        120
    );
}

#[cfg(unix)]
fn terminal_search(home: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::process::Stdio;
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty initializes both descriptors; no termios/window override.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: the descriptors are valid, distinct, and each is owned only here.
    let (mut master, slave) =
        unsafe { (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave)) };
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("ah"));
    cmd.current_dir(home)
        .env("HOME", home)
        .env("AH_PAGER", "cat")
        .arg("search")
        .args(args);
    for key in [
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "COPILOT_HOME",
        "CURSOR_DATA_DIR",
        "GROK_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "NO_COLOR",
        "AH_COLOR",
    ] {
        cmd.env_remove(key);
    }
    cmd.envs(env.iter().copied());
    let child = cmd
        .stdin(Stdio::null())
        .stdout(slave)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(cmd); // Close the parent's slave so reading the master can reach EOF.
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break, // Linux PTY EOF
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => panic!("PTY read: {e}"),
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(bytes).unwrap().replace("\r\n", "\n")
}

#[cfg(unix)]
#[test]
fn search_terminal_colors_layout_and_pager_obey_color_controls() {
    let tmp = fixture();
    for args in [
        vec!["needle", "--max-matches", "1", "--no-pager"],
        vec!["needle", "--max-matches", "1"],
    ] {
        let out = terminal_search(tmp.path(), &args, &[]);
        assert!(out.starts_with("\x1b[1m\x1b[36msession one"), "{out:?}");
        assert!(
            out.contains("\n\n    user        認証 \x1b[1;33mneedle\x1b[0m twice NEEDLE"),
            "{out:?}"
        );
        let plain = terminal_search(tmp.path(), &args, &[("NO_COLOR", "1")]);
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains("\n\n    user        認証 needle twice NEEDLE"));
        assert!(!plain.contains("bytes"));
        assert!(!plain.contains(".jsonl"));
        assert!(!plain.contains("#2"));
    }
    for args in [vec!["needle", "--no-color"], vec!["needle", "--color"]] {
        let out = terminal_search(tmp.path(), &args, &[("NO_COLOR", "1")]);
        assert!(!out.contains('\x1b')); // Existing NO_COLOR precedence is preserved.
    }
    let out = terminal_search(tmp.path(), &["needle", "--no-color"], &[]);
    assert!(!out.contains('\x1b'));
}

#[test]
fn search_structured_output_stays_plain_even_with_color_forced() {
    let tmp = fixture();
    for format in ["--json", "--tsv"] {
        let run = |extra: &[&str]| {
            ah(tmp.path())
                .args(["search", "needle", format])
                .args(extra)
                .assert()
                .success()
                .get_output()
                .stdout
                .clone()
        };
        let plain = run(&[]);
        assert_eq!(run(&["--color"]), plain);
        assert_eq!(run(&["--verbose"]), plain);
        assert_eq!(run(&["--color", "-v"]), plain);
        assert!(!plain.contains(&0x1b));
        assert!(!String::from_utf8(plain).unwrap().contains("_snippet_match"));
    }
    let output = ah(tmp.path())
        .args([
            "search",
            "needle",
            "--json",
            "--search-wire",
            "--max-matches",
            "1",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let wire: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(wire["_snippet_match"], json!([7, 13]));
}

#[cfg(unix)]
#[test]
fn search_compact_groups_passages_and_verbose_restores_locations() {
    let tmp = test_tempdir();
    let path = write_session(
        tmp.path(),
        "one",
        tmp.path(),
        &[
            json!({"type":"user", "message":{"content":"needle A"}}),
            json!({"type":"assistant", "message":{"content":"needle B"}}),
        ],
    );
    let older = write_session(
        tmp.path(),
        "two",
        tmp.path(),
        &[json!({"type":"user", "message":{"content":"needle C"}})],
    );
    fs::File::options()
        .write(true)
        .open(&older)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    let plain = terminal_search(tmp.path(), &["needle", "--no-color"], &[]);
    assert!(
        plain.contains("\n    user        needle A\n    assistant   needle B\n\nsession two "),
        "{plain:?}"
    );
    assert_eq!(plain.matches("session one ").count(), 1);
    assert!(!plain.contains("text #"));
    assert!(!plain.contains("bytes"));
    assert!(!plain.contains(&path));
    let verbose = terminal_search(tmp.path(), &["needle", "-v", "--no-color"], &[]);
    assert!(verbose.contains(&path));
    assert!(verbose.contains(&older));
    assert!(
        verbose.contains("text #2  bytes 0..6\n  at v1:"),
        "{verbose:?}"
    );
    assert!(
        verbose.contains("    user        needle A\n\n  text #3"),
        "{verbose:?}"
    );
    let colored = terminal_search(tmp.path(), &["needle", "--verbose"], &[]);
    assert!(colored.contains("\x1b[2mtext #2  bytes 0..6\x1b[0m"));
    assert!(colored.contains("\x1b[1;33mneedle\x1b[0m"));
}

fn show_at(home: &Path, hit: &Value, context: &str) -> Vec<Value> {
    let out = ah(home)
        .args([
            "show",
            hit["path"].as_str().unwrap(),
            "--at",
            hit["position"].as_str().unwrap(),
            "-C",
            context,
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

#[test]
fn kinds_filter_before_limits_and_keep_positions_identical() {
    let tmp = fixture();
    let all = search(tmp.path(), &["needle", "--max-matches", "0"]);
    assert_eq!(
        all.iter()
            .map(|h| h["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "user",
            "user",
            "assistant",
            "assistant",
            "tool-input",
            "tool-output"
        ]
    );
    for (kind, count) in [
        ("user", 2),
        ("assistant", 2),
        ("tool-input", 1),
        ("tool-output", 1),
        ("unknown", 0),
    ] {
        let hits = search(tmp.path(), &["needle", "--kind", kind]);
        assert_eq!(hits.len(), count);
        for h in &hits {
            assert!(
                all.iter()
                    .any(|original| original["position"] == h["position"]
                        && original["text_index"] == h["text_index"])
            );
            let source = show_at(tmp.path(), h, "0");
            let selected = source.iter().find(|v| v["selected"] == true).unwrap();
            assert_eq!(selected["kind"], kind);
            let text = selected["text"].as_str().unwrap();
            let start = h["match_start"].as_u64().unwrap() as usize;
            let end = h["match_end"].as_u64().unwrap() as usize;
            assert_eq!(text[start..end].to_lowercase(), "needle");
        }
    }
    assert_eq!(
        search(tmp.path(), &["needle", "-p"]),
        search(tmp.path(), &["needle", "--kind", "user"])
    );
    assert_eq!(
        search(
            tmp.path(),
            &["needle", "--kind", "tool-output", "--max-matches", "1"]
        )
        .len(),
        1
    );
    assert_eq!(
        search(tmp.path(), &["needle", "--kind", "user,assistant"]).len(),
        4
    );
    let tiny = search(tmp.path(), &["needle", "--snippet-length", "1"]);
    assert_eq!(all[0]["position"], tiny[0]["position"]);
    for args in [
        vec!["--kind", "bogus"],
        vec!["-p", "--kind", "assistant"],
        vec!["--kind", ""],
    ] {
        ah(tmp.path())
            .args(["search", "needle"])
            .args(args)
            .assert()
            .failure();
    }
}

#[test]
fn show_at_context_includes_tools_and_checks_staleness() {
    let tmp = fixture();
    let hit = search(tmp.path(), &["needle", "--kind", "tool-input"]).remove(0);
    let rows = show_at(tmp.path(), &hit, "1");
    assert_eq!(rows.iter().filter(|v| v["selected"] == true).count(), 1);
    assert!(rows.iter().any(|v| v["kind"] == "user"));
    assert!(rows.iter().any(|v| v["kind"] == "assistant"));
    assert!(rows.iter().any(|v| v["kind"] == "tool-output"));
    assert!(!rows.iter().any(|v| v["text"] == "start"));
    let path = hit["path"].as_str().unwrap();
    let token = hit["position"].as_str().unwrap();
    ah(tmp.path())
        .args(["show", path, "--at", token, "-C", "0", "--no-color"])
        .assert()
        .success()
        .stdout(predicate::str::contains("> [tool-input]"))
        .stdout(predicate::str::contains("echo needle"));
    let mut content = fs::read_to_string(path).unwrap();
    content.push_str("\n{\"type\":\"assistant\",\"message\":{\"content\":\"appended\"}}\n");
    fs::write(path, &content).unwrap();
    assert!(
        show_at(tmp.path(), &hit, "0")
            .iter()
            .any(|v| v["selected"] == true)
    );
    fs::write(path, content.replace("echo needle", "echo changed")).unwrap();
    ah(tmp.path())
        .args(["show", path, "--at", token])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("stale"));
}

#[test]
fn show_at_unicode_zero_width_and_invalid_options() {
    let tmp = fixture();
    for query in ["認証", "^", "$", "NEEDLE"] {
        let hits = search(tmp.path(), &[query, "--kind", "user"]);
        for h in &hits {
            assert!(
                show_at(tmp.path(), h, "0")
                    .iter()
                    .any(|v| v["selected"] == true)
            );
        }
    }
    let hit = search(tmp.path(), &["認証"]).remove(0);
    let path = hit["path"].as_str().unwrap();
    let token = hit["position"].as_str().unwrap();
    for extra in ["--raw", "--follow", "--tsv"] {
        ah(tmp.path())
            .args(["show", path, "--at", token, extra])
            .assert()
            .failure();
    }
    for extra in [
        ["--head", "1"],
        ["-o", "title"],
        ["--highlight", "x"],
        ["-C", "1001"],
    ] {
        ah(tmp.path())
            .args(["show", path, "--at", token])
            .args(extra)
            .assert()
            .failure();
    }
    ah(tmp.path())
        .args(["show", path, "-C", "2"])
        .assert()
        .failure();
    for bad in ["1", "v2:1:1:0:1:abcd", "v1:0:0:0:0:a", "v1:1:1:5:1:a"] {
        ah(tmp.path())
            .args(["show", path, "--at", bad])
            .assert()
            .failure();
    }
    // A valid digest does not authorize splitting a UTF-8 codepoint.
    let parts: Vec<_> = token.split(':').collect();
    let malformed = format!("v1:{}:{}:1:2:{}", parts[1], parts[2], parts[5]);
    ah(tmp.path())
        .args(["show", path, "--at", &malformed])
        .assert()
        .failure()
        .stdout("");
}

#[cfg(unix)]
#[test]
fn interactive_search_selects_occurrence_and_handles_cancel() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = fixture();
    let selector = tmp.path().join("selector");
    fs::write(&selector, "#!/bin/sh\nsed -n '1p'\n").unwrap();
    fs::set_permissions(&selector, fs::Permissions::from_mode(0o755)).unwrap();
    ah(tmp.path())
        .args([
            "search",
            "needle",
            "--kind",
            "tool-output",
            "-i",
            "-s",
            selector.to_str().unwrap(),
            "--no-pager",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("> [tool-output]"))
        .stdout(predicate::str::contains("output needle"));
    fs::write(&selector, "#!/bin/sh\ncat >/dev/null\nexit 130\n").unwrap();
    ah(tmp.path())
        .args(["search", "needle", "-i", "-s", selector.to_str().unwrap()])
        .assert()
        .success()
        .stdout("");
    fs::write(&selector, "#!/bin/sh\ncat >/dev/null\nprintf 'not-a-row'\n").unwrap();
    ah(tmp.path())
        .args(["search", "needle", "-i", "-s", selector.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid search selection"));
}

#[cfg(unix)]
#[test]
fn interactive_preview_roundtrips_hostile_paths_without_shell_evaluation() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = test_tempdir();
    let home = tmp.path().join("space ' $(touch HACKED)\t\nfolder");
    fs::create_dir_all(&home).unwrap();
    write_session(
        &home,
        "one",
        &home,
        &[json!({"type":"user", "message":{"content":"needle preview"}})],
    );
    let selector = tmp.path().join("fzf");
    fs::write(
        &selector,
        r#"#!/usr/bin/env python3
import os, shlex, subprocess, sys
rows = sys.stdin.read().splitlines()
row = rows[0]
fields = row.split('\t')
preview = next(arg.split('=', 1)[1] for arg in sys.argv[1:] if arg.startswith('--preview='))
preview = preview.replace('{1}', shlex.quote(fields[0])).replace('{2}', shlex.quote(fields[1]))
result = subprocess.run(['/bin/sh', '-c', preview], capture_output=True, text=True)
assert result.returncode == 0, result.stderr
assert '> [user]' in result.stdout and 'needle preview' in result.stdout, result.stdout
print(row)
"#,
    )
    .unwrap();
    fs::set_permissions(&selector, fs::Permissions::from_mode(0o755)).unwrap();
    ah(&home)
        .args([
            "search",
            "needle",
            "-i",
            "-s",
            selector.to_str().unwrap(),
            "--no-color",
            "--no-pager",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("needle preview"));
    assert!(!home.join("HACKED").exists());
}

#[cfg(unix)]
#[test]
fn remote_kind_filter_and_navigation_use_the_same_source_position() {
    use std::os::unix::fs::PermissionsExt;
    let local = test_tempdir();
    let remote = fixture();
    let bin = local.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let binary = assert_cmd::cargo::cargo_bin("ah");
    let config = format!(
        "[remotes.test]\nhost = 'example'\nah_path = '{}'\n",
        binary.display()
    );
    fs::write(local.path().join(".ahrc"), config).unwrap();
    let script = bin.join("ssh");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import os, shlex, sys
args = shlex.split(sys.argv[-1])
os.environ['HOME'] = os.environ['TEST_REMOTE_HOME']
os.chdir(os.environ['HOME'])
os.execv(args[0], args)
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let make = || {
        let mut c = ah(local.path());
        c.env("PATH", &path).env("TEST_REMOTE_HOME", remote.path());
        c
    };
    let output = make()
        .args([
            "search",
            "needle",
            "--remote",
            "test",
            "--kind",
            "tool-output",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hit: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(hit["kind"], "tool-output");
    assert!(hit["path"].as_str().unwrap().starts_with("test:"));
    let output = make()
        .args([
            "show",
            hit["path"].as_str().unwrap(),
            "--at",
            hit["position"].as_str().unwrap(),
            "-C",
            "0",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let selected: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(selected["kind"], "tool-output");
    assert_eq!(selected["selected"], true);
    assert_eq!(selected["text"], "output needle");
}

#[test]
fn user_parts_share_locations_and_changed_duplicate_prefix_is_rejected() {
    let tmp = test_tempdir();
    let path = write_session(
        tmp.path(),
        "one",
        tmp.path(),
        &[
            json!({"type":"user","message":{"content":[
                {"type":"text","text":"first part"},
                {"type":"text","text":"needle later part"}
            ]}}),
            json!({"type":"user","message":{"content":"needle duplicate"}}),
            json!({"type":"user","message":{"content":"needle duplicate"}}),
            json!({"type":"user","message":{"content":"needle duplicate"}}),
        ],
    );
    let all = search(tmp.path(), &["needle"]);
    let prompts = search(tmp.path(), &["needle", "-p"]);
    assert_eq!(all, prompts);
    assert_eq!(all.len(), 4);
    let hit = &all[2];
    let original = fs::read_to_string(&path).unwrap();
    let mut lines: Vec<_> = original.lines().collect();
    lines.remove(1);
    fs::write(&path, lines.join("\n")).unwrap();
    ah(tmp.path())
        .args(["show", &path, "--at", hit["position"].as_str().unwrap()])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("stale"));
}

#[test]
fn show_at_highlights_only_the_selected_occurrence_and_preserves_plain_show() {
    let tmp = fixture();
    let hits = search(tmp.path(), &["needle", "--kind", "user"]);
    let second = &hits[1];
    let path = second["path"].as_str().unwrap();
    let token = second["position"].as_str().unwrap();
    ah(tmp.path())
        .args(["show", path, "--at", token, "-C", "0", "--color"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "認証 needle twice \x1b[1;33mNEEDLE\x1b[0m",
        ));
    ah(tmp.path())
        .args(["show", path, "--at", token, "-C", "0", "--color"])
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b").not());
    let raw = ah(tmp.path())
        .args(["show", path, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    for line in String::from_utf8(raw).unwrap().lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        assert!(value.get("role").is_some());
        assert!(value.get("kind").is_none());
        assert!(value.get("selected").is_none());
    }
}

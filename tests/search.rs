use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

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
    let tmp = tempfile::tempdir().unwrap();
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
    let tmp = tempfile::tempdir().unwrap();
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
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        vec![],
        vec![""],
        vec!["["],
        vec!["x", "-q", "y"],
        vec!["x", "--raw-search"],
        vec!["x", "-i"],
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
    let tmp = tempfile::tempdir().unwrap();
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
    assert_eq!(cols.len(), 8);
    assert_eq!(cols[7], "needle\\nnext\\twith\\\\literal");
    assert!(Path::new(cols[0]).is_file());
}

#[test]
fn search_empty_home_is_success() {
    let tmp = tempfile::tempdir().unwrap();
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
        "text_index":2, "match_start":0, "match_end":6, "snippet":"needle", "_snippet_match":[0,6]});
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
    let tmp = tempfile::tempdir().unwrap();
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
    let tmp = tempfile::tempdir().unwrap();
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
                std::ptr::null(),
                std::ptr::null(),
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
            out.contains("\n\n    認証 \x1b[1;33mneedle\x1b[0m twice NEEDLE"),
            "{out:?}"
        );
        let plain = terminal_search(tmp.path(), &args, &[("NO_COLOR", "1")]);
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains("\n\n    認証 needle twice NEEDLE"));
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
    let tmp = tempfile::tempdir().unwrap();
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
        plain.contains("\n    needle A\n    needle B\n\nsession two "),
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
        verbose
            .contains("text #2  bytes 0..6\n    needle A\n\n  text #3  bytes 0..6\n    needle B"),
        "{verbose:?}"
    );
    let colored = terminal_search(tmp.path(), &["needle", "--verbose"], &[]);
    assert!(colored.contains("\x1b[2mtext #2  bytes 0..6\x1b[0m"));
    assert!(colored.contains("\x1b[1;33mneedle\x1b[0m"));
}

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ID: &str = "task_e_00000000000000000000000000000042";

fn pipe_ref(cmd: &mut Command, value: &str) -> std::process::Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{value}").unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn cached_cloud_sessions_use_normal_log_search_and_show_but_cannot_resume() {
    let home = tempfile::tempdir().unwrap();
    let codex = home.path().join(".codex");
    std::fs::create_dir(&codex).unwrap();
    let auth = |owner: &str| {
        std::fs::write(
            codex.join("auth.json"),
            json!({"auth_mode":"chatgpt","tokens":{
                "access_token":"fixture-only", "account_id":"workspace","login_owner_id":owner
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::set_permissions(
            codex.join("auth.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    };
    auth("owner");
    let key = Sha256::digest(b"owner\0workspace");
    let root = home.path().join(".ah-codex-cloud");
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    let cache = root.join(hex);
    for dir in [&root, &cache] {
        std::fs::DirBuilder::new().mode(0o700).create(dir).unwrap();
    }
    let entry = cache.join(format!("{ID}.jsonl"));
    let snapshot = [
        json!({"type":"session_meta","payload":{"id":ID,"cloud_title":"Example","cloud_created_at":2.0}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
            "ah_cloud_text":true,"content":[{"type":"input_text","text":"# Cloud example prompt"}]}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant",
            "ah_cloud_text":true,"content":[{"type":"output_text","text":"Cloud response"}]}}),
    ].map(|row| row.to_string()).join("\n");
    std::fs::write(&entry, snapshot).unwrap();
    symlink(
        &entry,
        cache.join("task_e_00000000000000000000000000000043.jsonl"),
    )
    .unwrap();

    let enable_cloud = |enabled| {
        std::fs::write(
            home.path().join(".ahrc"),
            format!("[cloud.codex]\nenabled = {enabled}\n"),
        )
        .unwrap();
    };
    let call = |args: &[&str], enabled: bool| {
        enable_cloud(enabled);
        Command::new(env!("CARGO_BIN_EXE_ah"))
            .env("HOME", home.path())
            .env("CODEX_HOME", &codex)
            .env_remove("AH_NO_CLOUD_SYNC")
            .current_dir(home.path())
            .args(args)
            .output()
            .unwrap()
    };
    let out = call(
        &[
            "-a",
            "-q",
            "Cloud example",
            "log",
            "--offline",
            "--json",
            "-o",
            "agent,id,origin,coverage,url,project,resume_cmd",
        ],
        true,
    );
    assert!(out.status.success(), "{:?}", out.stderr);
    let info: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["agent"], "codex");
    assert_eq!(info["id"], ID);
    assert_eq!(info["origin"], "cloud");
    assert_eq!(info["coverage"], "latest-turn");
    assert_eq!(info["url"], format!("https://chatgpt.com/codex/tasks/{ID}"));
    assert_eq!(info["project"], "");
    assert_eq!(info["resume_cmd"], "");

    let url = format!("https://chatgpt.com/codex/tasks/{ID}");
    let out = call(&["show", "--offline", "--json", &url], true);
    assert!(out.status.success(), "{:?}", out.stderr);
    let show = String::from_utf8(out.stdout).unwrap();
    assert!(show.contains("# Cloud example prompt") && show.contains("Cloud response"));
    for dash in [false, true] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ah"));
        cmd.env("HOME", home.path())
            .env("CODEX_HOME", &codex)
            .current_dir(home.path())
            .args(["show", "--offline", "--json"]);
        if dash {
            cmd.arg("-");
        }
        let out = pipe_ref(&mut cmd, &url);
        assert!(out.status.success(), "{:?}", out.stderr);
        assert_eq!(out.stdout, show.as_bytes());
    }
    let relative = entry.strip_prefix(home.path()).unwrap().to_str().unwrap();
    let out = call(&["show", "--offline", "--json", relative], true);
    assert!(out.status.success());
    assert_eq!(out.stdout, show.as_bytes());
    let basename = Command::new(env!("CARGO_BIN_EXE_ah"))
        .env("HOME", home.path())
        .env("CODEX_HOME", &codex)
        .current_dir(&cache)
        .args([
            "show",
            "--offline",
            "--json",
            entry.file_name().unwrap().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(basename.status.success());
    assert_eq!(basename.stdout, show.as_bytes());
    assert!(!call(&["resume", "--print", ID], true).status.success());
    assert!(!call(&["show", "--offline", ID], false).status.success());
    let outside_alias = home.path().join("legacy-cache-alias.jsonl");
    symlink(&entry, &outside_alias).unwrap();
    auth("another-owner");
    for path in [&entry, &outside_alias] {
        let denied = call(&["show", "--raw", path.to_str().unwrap()], true);
        assert!(!denied.status.success(), "{:?}", denied);
        assert!(denied.stdout.is_empty());
    }
    auth("owner");

    enable_cloud(true);
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_ah"))
        .env("HOME", home.path())
        .env("CODEX_HOME", &codex)
        .env_remove("AH_NO_CLOUD_SYNC")
        .args(["mcp", "-a", "--agent", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = mcp.stdin.take().unwrap();
    let mut output = BufReader::new(mcp.stdout.take().unwrap());
    let mut call_mcp = |id| {
        let req = json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":"log","arguments":{"fields":["id"]}}});
        writeln!(input, "{req}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], id);
        assert!(parsed.get("error").is_none(), "{line}");
        line
    };
    assert!(call_mcp(1).contains(ID));
    auth("another-owner");
    assert!(!call_mcp(2).contains(ID));
    drop(input);
    assert!(mcp.wait().unwrap().success());

    assert!(!call(&["show", "--offline", ID], true).status.success());
}

#[test]
fn offline_log_does_not_attempt_cloud_authentication() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".ahrc");
    let run = |offline| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ah"));
        cmd.env("HOME", home.path())
            .env("CODEX_HOME", home.path().join("no-login"))
            .env_remove("AH_NO_CLOUD_SYNC")
            .args(["log", "-a", "--json"]);
        if offline {
            cmd.arg("--offline");
        }
        String::from_utf8(cmd.output().unwrap().stderr).unwrap()
    };
    assert!(!run(false).contains("cannot read Codex file login"));
    std::fs::write(&config, "[cloud.codex]\nenabled = false\n").unwrap();
    assert!(!run(false).contains("cannot read Codex file login"));
    std::fs::write(&config, "[cloud.codex]\nenabled = true\n").unwrap();
    assert!(run(false).contains("cannot read Codex file login"));
    assert!(!run(true).contains("cannot read Codex file login"));
}

#[test]
fn cloud_conversations_scope_cache_and_distinguish_local_id_collisions() {
    const THREAD: &str = "11111111-1111-1111-1111-111111111111";
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let codex = home.join(".codex");
    std::fs::create_dir(&codex).unwrap();
    let auth = |account: &str| {
        let path = codex.join("auth.json");
        std::fs::write(
            &path,
            json!({"auth_mode":"chatgpt","tokens":{
                "access_token":"fixture-only", "account_id":account,"login_owner_id":"owner"
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    auth("first");
    let helper = home.join("bridge");
    let trace = home.join("trace");
    std::fs::write(&helper, format!(r#"#!/bin/sh
req=$(cat)
printf '%s\n' "$req" >> '{}'
case "$req" in
  *'"account":"first"'*'"id":null'*)
    printf '%s' '{{"data":[{{"id":"{THREAD}","name":"Example Cloud","created":1700000000,"updated":1700000001}}],"nextCursor":null}}'
    ;;
  *'"account":"first"'*'"id":"{THREAD}"'*)
    printf '%s' '{{"id":"{THREAD}","messages":[{{"role":"user","text":"first question"}},{{"role":"assistant","text":"first answer"}},{{"role":"user","text":"follow-up marker"}},{{"role":"assistant","text":"second answer"}}],"nextCursor":null}}'
    ;;
  *) exit 1;;
esac
"#, trace.display())).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.join(".ahrc"),
        format!(
            "[cloud.codex_conversations]\nenabled = true\nhelper = {:?}\n",
            helper
        ),
    )
    .unwrap();
    let call = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ah"))
            .env("HOME", home)
            .env("CODEX_HOME", &codex)
            .env_remove("AH_NO_CLOUD_SYNC")
            .current_dir(home)
            .args(args)
            .output()
            .unwrap()
    };
    let url = format!("https://chatgpt.com/local/{THREAD}");
    assert!(!call(&["show", "--offline", &url]).status.success());
    assert!(!trace.exists());
    let shown = call(&["show", "--json", &url]);
    assert!(shown.status.success(), "{:?}", shown.stderr);
    assert!(String::from_utf8_lossy(&shown.stdout).contains("second answer"));
    let calls = std::fs::read_to_string(&trace).unwrap();
    assert_eq!(calls.lines().count(), 1);
    assert!(!calls.contains("fixture-only"));
    let logged = call(&[
        "-a",
        "-q",
        "follow-up marker",
        "log",
        "--offline",
        "--json",
        "-o",
        "agent,id,origin,coverage",
    ]);
    assert!(logged.status.success(), "{:?}", logged.stderr);
    let log: Value = serde_json::from_slice(&logged.stdout).unwrap();
    assert_eq!(log["id"], THREAD);
    assert_eq!(log["agent"], "codex");
    assert_eq!(log["origin"], "cloud");
    assert_eq!(log["coverage"], "messages");
    assert_eq!(std::fs::read_to_string(&trace).unwrap(), calls);
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_ah"))
        .env("HOME", home)
        .env("CODEX_HOME", &codex)
        .current_dir(home)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = mcp.stdin.take().unwrap();
    let mut output = BufReader::new(mcp.stdout.take().unwrap());
    for id in [1, 2] {
        let req = json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":"log","arguments":{"fields":["id"]}}});
        writeln!(input, "{req}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert!(
            !line.contains(THREAD),
            "new provider cache must not be exposed via MCP"
        );
        auth("second");
    }
    drop(input);
    assert!(mcp.wait().unwrap().success());
    auth("first");
    let info = call(&["-a", "log", "--offline", "--json", "-o", "path"]);
    let row: Value = serde_json::from_slice(&info.stdout).unwrap();
    let cloud_path = std::path::Path::new(row["path"].as_str().unwrap());
    let canonical_home = home.canonicalize().unwrap();
    let relative = cloud_path
        .strip_prefix(&canonical_home)
        .unwrap()
        .to_str()
        .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ah"));
    cmd.env("HOME", home)
        .env("CODEX_HOME", &codex)
        .current_dir(home)
        .args(["show", "--json", "-"]);
    let piped = pipe_ref(&mut cmd, &url);
    assert!(piped.status.success(), "{:?}", piped.stderr);
    assert_eq!(shown.stdout, piped.stdout);
    let unknown = pipe_ref(
        &mut cmd,
        "https://chatgpt.com/local/22222222-2222-2222-2222-222222222222",
    );
    assert!(!unknown.status.success());
    assert_eq!(std::fs::read_to_string(&trace).unwrap(), calls);
    let showed_relative = call(&["show", "--offline", "--json", relative]);
    assert!(showed_relative.status.success());
    assert!(String::from_utf8_lossy(&showed_relative.stdout).contains("second answer"));

    let refreshed = call(&["-a", "log", "--refresh", "--json", "-o", "id,origin"]);
    assert!(refreshed.status.success(), "{:?}", refreshed.stderr);
    assert!(String::from_utf8_lossy(&refreshed.stdout).contains(THREAD));

    let local = codex.join("sessions/2026/11/22");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join(format!("rollout-2026-11-22T12-00-00-{THREAD}.jsonl")),
        [
            json!({"type":"session_meta","payload":{"id":THREAD,"cwd":"/local"}}),
            json!({"type":"response_item","payload":{"role":"user","type":"message","content":[{"type":"input_text","text":"local marker"}]}}),
        ].map(|row| row.to_string()).join("\n")
    ).unwrap();
    let listed = call(&["-a", "log", "--offline", "--json", "-o", "id,origin"]);
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout)
            .matches(THREAD)
            .count(),
        2
    );
    let ambiguous = call(&["show", "--offline", THREAD]);
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("Ambiguous"));
    let printed = call(&["resume", "--print", &url]);
    assert!(printed.status.success(), "{:?}", printed.stderr);
    assert!(String::from_utf8_lossy(&printed.stdout).contains(&url));

    auth("second");
    assert!(!call(&["show", "--offline", &url]).status.success());
    let listed = call(&["-a", "log", "--offline", "--json", "-o", "id,origin"]);
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout)
            .matches(THREAD)
            .count(),
        1
    );
}

#[test]
fn long_untitled_codex_threads_are_fetched_over_refreshes() {
    const THREAD: &str = "33333333-3333-3333-3333-333333333333";
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let codex = home.join(".codex");
    std::fs::create_dir(&codex).unwrap();
    let auth = codex.join("auth.json");
    std::fs::write(
        &auth,
        json!({"auth_mode":"chatgpt","tokens":{
            "access_token":"fixture-only", "account_id":"first","login_owner_id":"owner"
        }})
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    // An untitled thread whose transcript spans 40 pages (a browser run is
    // mostly non-message items): one message on the first page, and one on
    // the last. Cursors are page numbers.
    let helper = home.join("bridge");
    std::fs::write(&helper, format!(r#"#!/bin/sh
req=$(cat)
case "$req" in
  *'"id":null'*)
    printf '%s' '{{"data":[{{"id":"{THREAD}","name":"","created":1700000000,"updated":1700000100}}],"nextCursor":null}}'
    ;;
  *'"cursor":null'*)
    printf '%s' '{{"id":"{THREAD}","messages":[{{"role":"user","text":"\nbrowse the admin page"}}],"nextCursor":"1"}}'
    ;;
  *'"cursor":"39"'*)
    printf '%s' '{{"id":"{THREAD}","messages":[{{"role":"assistant","text":"final browsing report"}}],"nextCursor":null}}'
    ;;
  *)
    n=$(printf '%s' "$req" | sed 's/.*"cursor":"\([0-9]*\)".*/\1/')
    printf '{{"id":"{THREAD}","messages":[],"nextCursor":"%s"}}' $((n + 1))
    ;;
esac
"#)).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.join(".ahrc"),
        format!("[cloud.codex_conversations]\nenabled = true\nhelper = {helper:?}\n"),
    )
    .unwrap();
    let log = || {
        let out = Command::new(env!("CARGO_BIN_EXE_ah"))
            .env("HOME", home)
            .env("CODEX_HOME", &codex)
            .env_remove("AH_NO_CLOUD_SYNC")
            .current_dir(home)
            .args([
                "-a",
                "log",
                "--refresh",
                "--json",
                "-o",
                "id,coverage,title,transcript",
            ])
            .output()
            .unwrap();
        let row: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(row["id"], THREAD);
        row
    };
    // The first refresh reads 32 pages and keeps them; the title is the
    // first prompt, not empty.
    let first = log();
    assert_eq!(first["coverage"], "messages-partial");
    assert_eq!(
        first["title"],
        "[cloud:messages-partial] browse the admin page"
    );
    assert!(
        !first["transcript"]
            .as_str()
            .unwrap()
            .contains("final browsing report")
    );
    // The next refresh continues where it stopped instead of failing again.
    let second = log();
    assert_eq!(second["coverage"], "messages");
    let transcript = second["transcript"].as_str().unwrap();
    assert!(transcript.contains("browse the admin page"), "{transcript}");
    assert!(transcript.contains("final browsing report"), "{transcript}");
}

#[test]
fn claude_cloud_resume_prints_only_the_selected_session_link() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(".ahrc"), "[cloud.claude]\nenabled = true\n").unwrap();
    let url = "https://claude.ai/code/session_01example";
    let out = Command::new(env!("CARGO_BIN_EXE_ah"))
        .env("HOME", tmp.path())
        .args(["resume", "--print", url])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out.stderr);
    assert!(String::from_utf8_lossy(&out.stdout).contains(url));
}

#[test]
fn claude_explicit_cloud_export_is_scoped_and_never_teleports_from_log() {
    const ID: &str = "session_testClaudeCloud012";
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let helper = home.join("helper");
    let requests = home.join("requests");
    std::fs::write(&helper, format!(r#"#!/bin/sh
req=$(cat)
printf '%s\n' "$req" >> '{}'
case "$req" in
  *'"identity":true'*)
    printf '{{"identity":["%s","fixture-org"]}}' "$MOCK_CLAUDE_ACCOUNT"
    ;;
  *'"account":["first","fixture-org"]'*'"id":"{ID}"'*)
    if test -f fail-export; then exit 1; fi
    extra=
    if test -f new-turn; then extra=',{{"role":"assistant","text":"a new turn"}}'; fi
    printf '%s%s%s' '{{"id":"{ID}","messages":[{{"role":"user","text":"first Claude question"}},{{"role":"assistant","text":"first response"}},{{"role":"user","text":"Claude follow-up fixture"}}' "$extra" ']}}'
    ;;
  *) exit 1;;
esac
"#, requests.display())).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.join(".ahrc"),
        format!("[cloud]\nunknown = 1\n[cloud.claude]\nenabled = true\nhelper = {helper:?}\nrepo = {home:?}\n"),
    )
    .unwrap();
    let call = |account: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ah"))
            .env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("MOCK_CLAUDE_ACCOUNT", account)
            .env_remove("AH_NO_CLOUD_SYNC")
            .args(["-a", "--agent", "claude"])
            .args(args)
            .current_dir(home)
            .output()
            .unwrap()
    };
    let url = format!("https://claude.ai/code/{ID}");
    assert!(!call("first", &["show", "--offline", &url]).status.success());
    assert!(!requests.exists());
    let empty = call("first", &["log", "--json"]);
    assert!(empty.stdout.is_empty(), "{:?}", empty);
    assert!(!String::from_utf8_lossy(&empty.stderr).contains("ignoring invalid"));
    assert_eq!(
        std::fs::read_to_string(&requests).unwrap().lines().count(),
        1
    );
    let shown = call("first", &["show", "--json", &url]);
    assert!(shown.status.success(), "{:?}", shown.stderr);
    assert!(String::from_utf8_lossy(&shown.stdout).contains("Claude follow-up fixture"));
    let args = [
        "-q",
        "follow-up fixture",
        "log",
        "--json",
        "-o",
        "id,origin,coverage",
    ];
    let found = call("first", &args);
    assert_eq!(
        serde_json::from_slice::<Value>(&found.stdout).unwrap(),
        json!({"id":ID,"origin":"cloud","coverage":"teleport-export"}),
    );
    let detail_search = call("first", &["search", "follow-up fixture", "--json"]);
    assert!(detail_search.status.success(), "{:?}", detail_search.stderr);
    assert!(String::from_utf8_lossy(&detail_search.stdout).contains(ID));
    let hits = |text, flags: &[&str]| {
        let mut args = vec!["search", text, "--json"];
        args.extend_from_slice(flags);
        let out = call("first", &args);
        assert!(out.status.success(), "{:?}", out.stderr);
        out.stdout
    };
    let prompt = hits("follow-up fixture", &["-p"]);
    let found: Value = serde_json::from_slice(&prompt).unwrap();
    assert_eq!(found["kind"], "user");
    let reply = hits("first response", &["--kind", "user,assistant"]);
    let found: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(found["kind"], "assistant");
    assert!(hits("first response", &["-p"]).is_empty());

    assert!(
        call("second", &["search", "follow-up fixture", "--json"])
            .stdout
            .is_empty()
    );
    assert!(call("second", &args).stdout.is_empty());
    let trace = std::fs::read_to_string(&requests).unwrap();
    assert_eq!(trace.lines().filter(|line| line.contains(ID)).count(), 1);
    let resume = call("first", &["resume", "--print", ID]);
    assert!(resume.status.success(), "{:?}", resume.stderr);
    assert!(String::from_utf8_lossy(&resume.stdout).contains(&url));

    let out = call("first", &["log", "--json", "-o", "path"]);
    let row: Value = serde_json::from_slice(&out.stdout).unwrap();
    let snapshot = std::path::Path::new(row["path"].as_str().unwrap());
    let cached_path = format!("path:{}", snapshot.display());
    let path_resume = call("first", &["resume", "--print", &cached_path]);
    assert!(path_resume.status.success(), "{:?}", path_resume.stderr);
    assert_eq!(resume.stdout, path_resume.stdout);
    let alias = home.join("outside-cache-alias.json");
    symlink(snapshot, &alias).unwrap();
    let canonical_home = home.canonicalize().unwrap();
    let relative = snapshot.strip_prefix(&canonical_home).unwrap();
    for path in [snapshot, &alias, relative] {
        for format in [
            vec!["--raw"],
            vec![],
            vec!["--json"],
            vec!["-o", "transcript"],
        ] {
            let mut args = vec!["show"];
            args.extend_from_slice(&format);
            args.push(path.to_str().unwrap());
            let other = call("second", &args);
            assert!(!other.status.success(), "{:?}", other);
            assert!(other.stdout.is_empty());
            args.insert(1, "--offline");
            let offline = call("first", &args);
            assert!(!offline.status.success(), "{:?}", offline);
            assert!(offline.stdout.is_empty());
        }
    }
    let stdin = |reference, dash| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ah"));
        cmd.env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("MOCK_CLAUDE_ACCOUNT", "first")
            .env_remove("AH_NO_CLOUD_SYNC")
            .current_dir(home)
            .args(["show", "--json"]);
        if dash {
            cmd.arg("-");
        }
        pipe_ref(&mut cmd, reference)
    };
    for reference in [&url[..], alias.to_str().unwrap()] {
        for dash in [false, true] {
            let piped = stdin(reference, dash);
            assert!(piped.status.success(), "{:?}", piped.stderr);
            assert_eq!(shown.stdout, piped.stdout);
        }
    }
    let missing = stdin("https://claude.ai/code/session_unknown", true);
    assert!(!missing.status.success());
    let trace = std::fs::read_to_string(&requests).unwrap();
    assert_eq!(
        trace
            .lines()
            .filter(|line| line.contains("\"id\":"))
            .count(),
        1
    );

    let read_snapshot =
        || serde_json::from_slice::<Value>(&std::fs::read(snapshot).unwrap()).unwrap();
    let modified = read_snapshot()["modified"].as_u64().unwrap();
    let recent = call("first", &["log", "--since", "1h", "--json", "-o", "id"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&recent.stdout).unwrap()["id"],
        ID
    );
    let recent_hits = call(
        "first",
        &["search", "follow-up fixture", "--since", "1h", "--json"],
    );
    assert!(String::from_utf8_lossy(&recent_hits.stdout).contains(ID));

    let expire = || {
        let mut data: Value = serde_json::from_slice(&std::fs::read(snapshot).unwrap()).unwrap();
        data["fetched"] = json!(1);
        std::fs::write(snapshot, data.to_string()).unwrap();
    };
    let old_change = modified - 120;
    let mut data = read_snapshot();
    data["modified"] = json!(old_change);
    data["fetched"] = json!(1);
    std::fs::write(snapshot, data.to_string()).unwrap();
    let same = call("first", &["show", "--json", &url]);
    assert_eq!(shown.stdout, same.stdout);
    assert_eq!(read_snapshot()["modified"], old_change);

    std::fs::write(home.join("new-turn"), "").unwrap();
    let fresh = call("first", &["show", "--json", &url]);
    assert_eq!(shown.stdout, fresh.stdout);
    expire();
    let refreshed = call("first", &["show", "--json", &url]);
    assert!(refreshed.status.success(), "{:?}", refreshed.stderr);
    assert!(String::from_utf8_lossy(&refreshed.stdout).contains("a new turn"));
    assert!(read_snapshot()["modified"].as_u64().unwrap() > old_change);
    assert!(!hits("a new turn", &["--kind", "assistant"]).is_empty());
    std::fs::write(home.join("fail-export"), "").unwrap();
    expire();
    let fallback = call("first", &["show", "--json", &url]);
    assert!(fallback.status.success(), "{:?}", fallback.stderr);
    assert_eq!(refreshed.stdout, fallback.stdout);
    assert!(String::from_utf8_lossy(&fallback.stderr).contains("not refreshed"));
}

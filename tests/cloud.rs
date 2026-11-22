#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ID: &str = "task_e_00000000000000000000000000000042";

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
    let cache = root.join(format!("{key:x}"));
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
        .unwrap()
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

//! `ah mcp`: the stdio MCP server. Batch tests pipe a whole script through
//! stdin; the bidirectional tests keep stdin open to exercise cancellation.
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION_ID: &str = "abc12345-0000-0000-0000-000000000001";
const SESSION_CWD: &str = "/Users/test/myproject";
const JP_SESSION_ID: &str = "abc12345-0000-0000-0000-000000000002";

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A home with one Claude session (the shared fixture), one Japanese
/// session, and a project directory holding memory files.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join(".claude/projects/-tmp-proj");
        fs::create_dir_all(&projects).unwrap();
        fs::copy(
            fixture_path("claude_session.jsonl"),
            projects.join(format!("{SESSION_ID}.jsonl")),
        )
        .unwrap();
        fs::write(
            projects.join(format!("{JP_SESSION_ID}.jsonl")),
            concat!(
                "{\"type\":\"system\",\"cwd\":\"/Users/test/jp\"}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"日本語のテキストです\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"了解しました。\"}]}}\n",
            ),
        )
        .unwrap();
        let project = dir.path().join("proj");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::write(project.join("CLAUDE.md"), "# Project rules\n\nUse tabs.\n").unwrap();
        fs::write(
            project.join("AGENTS.md"),
            "# Agents\n\nShared notes about oauth.\n",
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Canonical, as the server reports it (macOS puts tmp under /private).
    fn project(&self) -> String {
        fs::canonicalize(self.path().join("proj"))
            .unwrap()
            .display()
            .to_string()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ah"));
        cmd.arg("mcp")
            .args(args)
            .env("HOME", self.path())
            .env("CLAUDE_CONFIG_DIR", self.path().join(".claude"))
            .env("CODEX_HOME", "/nonexistent")
            .env("GEMINI_CLI_HOME", "/nonexistent")
            .env("CURSOR_DATA_DIR", "/nonexistent")
            .env("GROK_HOME", "/nonexistent")
            .env("XDG_DATA_HOME", "/nonexistent")
            .env_remove("AH_MCP_TEST_CHECKPOINT_DELAY_MS")
            .env_remove("AH_MCP_TEST_PANIC")
            .current_dir(self.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Run the server with `args`, feed `messages`, and return the parsed
    /// responses (every stdout line must be JSON).
    fn batch(&self, args: &[&str], messages: &[Value]) -> Vec<Value> {
        let mut child = self.command(args).spawn().unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            for m in messages {
                writeln!(stdin, "{}", m).unwrap();
            }
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "server failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        stdout
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line)
                    .unwrap_or_else(|e| panic!("non-JSON line on stdout: {line:?} ({e})"))
            })
            .collect()
    }
}

fn init() -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}})
}

fn initialized() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
}

fn call(id: u64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
}

fn by_id(responses: &[Value], id: u64) -> &Value {
    responses
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap_or_else(|| panic!("no response with id {id}: {responses:?}"))
}

/// The tool result of a successful (non-error) call.
fn structured(responses: &[Value], id: u64) -> &Value {
    let r = by_id(responses, id);
    assert!(
        r["error"].is_null(),
        "protocol error for id {id}: {}",
        r["error"]
    );
    assert_eq!(
        r["result"]["isError"],
        json!(false),
        "tool error for id {id}: {}",
        r["result"]["content"][0]["text"]
    );
    &r["result"]["structuredContent"]
}

fn tool_error(responses: &[Value], id: u64) -> &str {
    let r = by_id(responses, id);
    assert!(
        r["error"].is_null(),
        "protocol error for id {id}: {}",
        r["error"]
    );
    assert_eq!(
        r["result"]["isError"],
        json!(true),
        "expected tool error for id {id}: {r}"
    );
    assert!(
        r["result"].get("structuredContent").is_none(),
        "isError results carry no structuredContent"
    );
    r["result"]["content"][0]["text"].as_str().unwrap()
}

// ─── handshake and protocol ──────────────────────────────────────────

#[test]
fn initialize_echoes_supported_version_and_lists_tools() {
    let home = Home::new();
    let out = home.batch(
        &[],
        &[
            init(),
            initialized(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ],
    );
    let init = &by_id(&out, 0)["result"];
    assert_eq!(init["protocolVersion"], json!("2025-06-18"));
    assert_eq!(init["serverInfo"]["name"], json!("ah"));
    assert_eq!(init["capabilities"]["tools"]["listChanged"], json!(false));
    assert!(init["instructions"].as_str().unwrap().contains("scope"));
    assert_eq!(by_id(&out, 1)["result"], json!({}));
    let tools: Vec<&str> = by_id(&out, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "log",
            "search",
            "show",
            "project",
            "memory",
            "agent",
            "list_agents"
        ]
    );
    for t in by_id(&out, 2)["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["inputSchema"]["additionalProperties"], json!(false));
    }
}

#[test]
fn unknown_legacy_version_gets_newest_legacy_version() {
    let home = Home::new();
    let out = home.batch(
        &[],
        &[json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}})],
    );
    assert_eq!(
        by_id(&out, 0)["result"]["protocolVersion"],
        json!("2025-11-25")
    );
}

#[test]
fn modern_discover_and_version_errors() {
    let home = Home::new();
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}});
    let out = home.batch(
        &[],
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {"_meta": meta}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"_meta": meta}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"_meta": meta, "name": "list_agents"}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list",
                "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "1900-01-01"}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "ping", "params": {"_meta": meta}}),
            json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list",
                "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}}),
        ],
    );
    assert_eq!(by_id(&out, 5)["result"]["resultType"], json!("complete"));
    assert_eq!(
        by_id(&out, 6)["error"]["code"],
        json!(-32602),
        "modern requests must carry clientCapabilities"
    );
    let discover = &by_id(&out, 1)["result"];
    assert_eq!(discover["resultType"], json!("complete"));
    assert_eq!(discover["supportedVersions"], json!(["2026-07-28"]));
    assert_eq!(
        discover["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("ah")
    );
    assert!(discover["instructions"].is_string());
    let list = &by_id(&out, 2)["result"];
    assert_eq!(list["resultType"], json!("complete"));
    assert!(list["ttlMs"].is_number());
    assert_eq!(list["cacheScope"], json!("private"));
    assert_eq!(by_id(&out, 3)["result"]["resultType"], json!("complete"));
    let err = &by_id(&out, 4)["error"];
    assert_eq!(err["code"], json!(-32022));
    assert_eq!(err["data"]["supported"], json!(["2026-07-28"]));
    assert_eq!(err["data"]["requested"], json!("1900-01-01"));
    // Legacy results carry none of the modern fields.
    let legacy = home.batch(
        &[],
        &[
            init(),
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}),
        ],
    );
    assert!(by_id(&legacy, 5)["result"].get("resultType").is_none());
    assert_eq!(by_id(&legacy, 6)["result"], json!({}));
}

#[test]
fn protocol_errors() {
    let home = Home::new();
    let mut child = home.command(&[]).spawn().unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{}", init()).unwrap();
        writeln!(stdin, "not json").unwrap();
        writeln!(stdin, "[1,2]").unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 7, "method": "resources/list"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "nope"}})
        )
        .unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "log", "arguments": 3}})).unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/whatever"})
        )
        .unwrap();
        writeln!(stdin, "{}", json!({"id": 10, "method": "ping"})).unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "1.0", "id": 11, "method": "ping"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": [1], "method": "ping"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 13, "method": "ping", "params": [1]})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": null, "method": "ping"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 15, "method": "ping", "params": null})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 17, "method": 5})
        )
        .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let out: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let codes: Vec<i64> = out
        .iter()
        .filter(|r| r["id"].is_null())
        .map(|r| r["error"]["code"].as_i64().unwrap())
        .collect();
    assert_eq!(
        codes,
        [-32700, -32600, -32600, -32600, -32600],
        "parse error, array message, array id, null id, float id"
    );
    assert_eq!(
        by_id(&out, 15)["error"]["code"],
        json!(-32600),
        "params: null"
    );
    assert_eq!(
        by_id(&out, 17)["error"]["code"],
        json!(-32600),
        "method not a string"
    );
    assert_eq!(by_id(&out, 10)["error"]["code"], json!(-32600));
    assert_eq!(by_id(&out, 11)["error"]["code"], json!(-32600));
    assert_eq!(by_id(&out, 13)["error"]["code"], json!(-32600));
    assert_eq!(by_id(&out, 7)["error"]["code"], json!(-32601));
    assert_eq!(by_id(&out, 8)["error"]["code"], json!(-32602));
    assert!(
        by_id(&out, 8)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown tool")
    );
    assert_eq!(by_id(&out, 9)["error"]["code"], json!(-32602));
    assert_eq!(out.len(), 14, "notifications get no response: {out:?}");
}

#[test]
fn startup_rejects_incompatible_global_flags() {
    let home = Home::new();
    for args in [
        vec!["-q", "x"],
        vec!["-p"],
        vec!["--raw-search"],
        vec!["-i"],
        vec!["--color"],
        vec!["--remote", "nowhere"],
        vec!["-A"],
    ] {
        let output = home.command(&args).output().unwrap();
        assert!(!output.status.success(), "ah mcp {args:?} should fail");
        assert!(output.stdout.is_empty(), "nothing on stdout for {args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ah mcp"),
            "stderr names the problem for {args:?}"
        );
    }
    // Harmless flags are accepted.
    let output = home
        .command(&["--no-color", "--no-pager"])
        .output()
        .unwrap();
    assert!(output.status.success());
}

// ─── log ─────────────────────────────────────────────────────────────

#[test]
fn log_scope_fields_and_limits() {
    let home = Home::new();
    let out = home.batch(
        &[],
        &[
            init(),
            call(1, "log", json!({"all": true})),
            call(
                2,
                "log",
                json!({"all": true, "max_results": 1, "fields": ["id", "cwd"]}),
            ),
            call(3, "log", json!({"dir": SESSION_CWD})),
            call(4, "log", json!({"dir": "/nonexistent/dir"})),
            call(5, "log", json!({"all": true, "dir": SESSION_CWD})),
            call(6, "log", json!({"all": true, "bogus": 1})),
            call(7, "log", json!({"all": true, "query": "auth bug"})),
            call(8, "log", json!({"all": true, "since": "yesterday"})),
            call(9, "log", json!({"all": true, "fields": ["nope"]})),
            call(10, "log", json!({})),
            call(11, "log", json!({"all": true, "max_results": 0})),
            call(
                12,
                "log",
                json!({"all": true, "sort": "turns", "order": "asc", "max_results": 1, "fields": ["id"]}),
            ),
            call(
                13,
                "log",
                json!({"all": true, "sort": "turns", "order": "desc", "max_results": 1, "fields": ["id"]}),
            ),
        ],
    );
    // The sort field is resolved even when it is not returned.
    assert_eq!(
        structured(&out, 12)["sessions"][0]["id"],
        json!(JP_SESSION_ID)
    );
    assert_eq!(structured(&out, 13)["sessions"][0]["id"], json!(SESSION_ID));
    assert!(structured(&out, 12)["sessions"][0].get("turns").is_none());
    let r1 = structured(&out, 1);
    assert_eq!(r1["scope"], json!({"dir": null, "all": true}));
    assert_eq!(r1["total"], json!(2));
    assert_eq!(r1["truncated"], json!(false));
    assert_eq!(r1["sessions"].as_array().unwrap().len(), 2);
    let first = &r1["sessions"][0];
    for key in ["agent", "project", "modified_at", "title", "id"] {
        assert!(first.get(key).is_some(), "default field {key}");
    }
    assert!(first.get("cwd").is_none(), "cwd is not a default field");

    let r2 = structured(&out, 2);
    assert_eq!(r2["total"], json!(2));
    assert_eq!(r2["truncated"], json!(true));
    assert_eq!(r2["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(r2["sessions"][0].as_object().unwrap().len(), 2);

    let r3 = structured(&out, 3);
    assert_eq!(r3["scope"], json!({"dir": SESSION_CWD, "all": false}));
    assert_eq!(r3["sessions"][0]["id"], json!(SESSION_ID));
    assert_eq!(r3["total"], json!(1));

    let r4 = structured(&out, 4);
    assert_eq!(r4["sessions"], json!([]));
    assert_eq!(r4["scope"]["dir"], json!("/nonexistent/dir"));

    assert!(tool_error(&out, 5).contains("exclusive"));
    assert!(tool_error(&out, 6).contains("bogus"));

    let r7 = structured(&out, 7);
    assert_eq!(r7["total"], json!(1));
    assert!(
        r7["sessions"][0]["matched"]
            .as_str()
            .unwrap()
            .contains("auth bug")
    );

    assert!(tool_error(&out, 8).contains("since"));
    assert!(tool_error(&out, 9).contains("unknown field"));

    // Default scope is the server cwd (the temp home), which has no sessions.
    let r10 = structured(&out, 10);
    assert_eq!(r10["scope"]["all"], json!(false));
    assert_eq!(r10["sessions"], json!([]));

    assert_eq!(
        structured(&out, 11)["sessions"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn global_flags_are_defaults_that_tool_arguments_override() {
    let home = Home::new();
    let out = home.batch(
        &["-a"],
        &[
            init(),
            call(1, "log", json!({})),
            call(2, "log", json!({"dir": SESSION_CWD})),
            call(3, "log", json!({"all": false})),
        ],
    );
    assert_eq!(structured(&out, 1)["scope"]["all"], json!(true));
    assert_eq!(structured(&out, 1)["total"], json!(2));
    // dir clears the inherited -a instead of being ignored.
    let r2 = structured(&out, 2);
    assert_eq!(r2["scope"], json!({"dir": SESSION_CWD, "all": false}));
    assert_eq!(r2["total"], json!(1));
    assert_eq!(structured(&out, 3)["scope"]["all"], json!(false));
    assert_eq!(structured(&out, 3)["total"], json!(0));

    let out = home.batch(
        &["--agent", "codex"],
        &[init(), call(1, "log", json!({"all": true}))],
    );
    assert_eq!(structured(&out, 1)["total"], json!(0));
    let out = home.batch(
        &["--agent", "codex"],
        &[
            init(),
            call(1, "log", json!({"all": true, "agent": "claude"})),
        ],
    );
    assert_eq!(structured(&out, 1)["total"], json!(2));
}

// ─── search and show ─────────────────────────────────────────────────

#[test]
fn search_then_show_at_position() {
    let home = Home::new();
    let out = home.batch(
        &[],
        &[
            init(),
            call(1, "search", json!({"all": true, "pattern": "auth"})),
            call(2, "search", json!({"all": true})),
            call(3, "search", json!({"all": true, "pattern": "("})),
            call(
                4,
                "search",
                json!({"all": true, "pattern": "the", "max_matches": 1}),
            ),
            call(
                5,
                "search",
                json!({"all": true, "pattern": "auth", "prompt_only": true, "kind": ["assistant"]}),
            ),
            call(
                6,
                "search",
                json!({"all": true, "pattern": "auth", "kind": ["bogus"]}),
            ),
            call(
                7,
                "search",
                json!({"all": true, "pattern": "auth", "kind": ["user"]}),
            ),
            call(
                8,
                "search",
                json!({"all": true, "pattern": "the", "max_matches": u64::MAX}),
            ),
        ],
    );
    assert_eq!(structured(&out, 8)["truncated"], json!(false));
    assert!(!structured(&out, 8)["hits"].as_array().unwrap().is_empty());
    let r1 = structured(&out, 1);
    assert_eq!(r1["scope"]["all"], json!(true));
    assert_eq!(r1["truncated"], json!(false));
    let hits = r1["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    let hit = &hits[0];
    for key in [
        "path",
        "agent",
        "project",
        "id",
        "modified_at",
        "title",
        "text_index",
        "match_start",
        "match_end",
        "snippet",
        "kind",
        "position",
    ] {
        assert!(hit.get(key).is_some(), "hit field {key}");
    }
    assert!(hit.get("_snippet_match").is_none());
    assert!(tool_error(&out, 2).contains("pattern"));
    assert!(tool_error(&out, 3).contains("Invalid regex"));
    let r4 = structured(&out, 4);
    assert_eq!(r4["hits"].as_array().unwrap().len(), 1);
    assert_eq!(r4["truncated"], json!(true));
    assert!(tool_error(&out, 5).contains("prompt_only"));
    assert!(tool_error(&out, 6).contains("unknown kind"));
    for h in structured(&out, 7)["hits"].as_array().unwrap() {
        assert_eq!(h["kind"], json!("user"));
    }

    // Open the first hit with context.
    let position = hit["position"].as_str().unwrap().to_string();
    let out = home.batch(
        &[],
        &[
            init(),
            call(
                1,
                "show",
                json!({"session": SESSION_ID, "at": position, "context": 1}),
            ),
            call(
                2,
                "show",
                json!({"session": SESSION_ID, "at": "1.1:0-1:deadbeef"}),
            ),
            call(3, "show", json!({"session": SESSION_ID, "context": 1})),
            call(
                4,
                "show",
                json!({"session": SESSION_ID, "at": position, "context": 2, "max_chars": 1}),
            ),
        ],
    );
    // max_chars drops the context farthest from the hit, never the hit.
    let r4 = structured(&out, 4);
    assert_eq!(r4["truncated"], json!(true));
    assert_eq!(r4["records"].as_array().unwrap().len(), 1);
    assert_eq!(r4["records"][0]["selected"], json!(true));
    let r1 = structured(&out, 1);
    assert_eq!(r1["session"]["id"], json!(SESSION_ID));
    assert_eq!(r1["session"]["agent"], json!("claude"));
    assert!(r1.get("scope").is_none(), "show has no scope");
    let records = r1["records"].as_array().unwrap();
    let selected: Vec<&Value> = records
        .iter()
        .filter(|r| r["selected"] == json!(true))
        .collect();
    assert_eq!(selected.len(), 1);
    assert!(selected[0]["match_start"].is_number());
    assert!(
        selected[0]["text"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("auth")
    );
    for r in records.iter().filter(|r| r["selected"] == json!(false)) {
        assert!(r["match_start"].is_null());
    }
    assert!(tool_error(&out, 2).contains("stale") || tool_error(&out, 2).contains("position"));
    assert!(tool_error(&out, 3).contains("context"));
}

#[test]
fn show_formats_and_truncation() {
    let home = Home::new();
    let out = home.batch(
        &[],
        &[
            init(),
            call(1, "show", json!({"session": SESSION_ID})),
            call(
                2,
                "show",
                json!({"session": SESSION_ID, "format": "json", "head": 1}),
            ),
            call(
                3,
                "show",
                json!({"session": JP_SESSION_ID, "max_chars": 12}),
            ),
            call(
                4,
                "show",
                json!({"session": SESSION_ID, "format": "json", "max_chars": 30}),
            ),
            call(5, "show", json!({"session": "no-such-session"})),
            call(6, "show", json!({"session": SESSION_ID, "format": "xml"})),
            call(7, "show", json!({})),
            call(8, "show", json!({"session": "abc12345"})),
        ],
    );
    let r1 = by_id(&out, 1);
    let md = r1["result"]["content"][0]["text"].as_str().unwrap();
    assert!(md.starts_with("## User\n\nfix the auth bug"), "{md}");
    assert!(md.contains("\n---\n\n## Assistant\n\n"));
    assert_eq!(r1["result"]["structuredContent"]["markdown"], json!(md));
    assert_eq!(r1["result"]["structuredContent"]["truncated"], json!(false));

    let r2 = structured(&out, 2);
    assert_eq!(
        r2["messages"],
        json!([{"role": "user", "text": "fix the auth bug"}])
    );
    assert_eq!(r2["truncated"], json!(false));

    // Cut on a character boundary, not a byte.
    let r3 = by_id(&out, 3);
    let s3 = &r3["result"]["structuredContent"];
    assert_eq!(s3["truncated"], json!(true));
    assert_eq!(s3["markdown"].as_str().unwrap().chars().count(), 12);
    assert_eq!(s3["markdown"], json!("## User\n\n日本語"));
    assert!(
        r3["result"]["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("[truncated")
    );
    assert!(
        r1["result"]["content"].as_array().unwrap().len() == 1,
        "no note block unless truncated"
    );

    let r4 = structured(&out, 4);
    assert_eq!(r4["truncated"], json!(true));
    assert_eq!(
        r4["messages"].as_array().unwrap().len(),
        1,
        "whole messages are dropped from the end"
    );

    assert!(tool_error(&out, 5).contains("No session found"));
    assert!(tool_error(&out, 6).contains("format"));
    assert!(tool_error(&out, 7).contains("session"));
    assert!(tool_error(&out, 8).contains("Ambiguous"));
}

// ─── project, memory, agent, list_agents ─────────────────────────────

#[test]
fn project_tool() {
    let home = Home::new();
    let out = home.batch(
        &[],
        &[
            init(),
            call(1, "project", json!({})),
            call(
                2,
                "project",
                json!({"project": "myproject", "fields": ["project", "cwd", "session_count"]}),
            ),
            call(3, "project", json!({"project": "nothing-here"})),
            call(4, "project", json!({"dir": "/nonexistent"})),
            call(5, "project", json!({"all": true})),
            call(
                6,
                "project",
                json!({"sort": "cwd", "order": "asc", "fields": ["project"]}),
            ),
            call(
                7,
                "project",
                json!({"sort": "cwd", "order": "desc", "fields": ["project"]}),
            ),
        ],
    );
    // Sorting by a field that is not returned still orders by its value.
    assert_eq!(structured(&out, 6)["projects"][0]["project"], json!("jp"));
    assert_eq!(
        structured(&out, 7)["projects"][0]["project"],
        json!("myproject")
    );
    assert!(structured(&out, 6)["projects"][0].get("cwd").is_none());
    let r1 = structured(&out, 1);
    assert_eq!(r1["scope"], json!({"dir": null, "all": true}));
    let names: Vec<&str> = r1["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["project"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"myproject"));
    let r2 = structured(&out, 2);
    assert_eq!(r2["projects"].as_array().unwrap().len(), 1);
    assert_eq!(r2["projects"][0]["cwd"], json!(SESSION_CWD));
    assert_eq!(r2["projects"][0]["session_count"], json!(1));
    assert_eq!(structured(&out, 3)["projects"], json!([]));
    let r4 = structured(&out, 4);
    assert_eq!(r4["projects"], json!([]));
    assert_eq!(r4["scope"]["dir"], json!("/nonexistent"));
    assert_eq!(
        structured(&out, 5)["scope"],
        json!({"dir": null, "all": true})
    );

    // all: true clears a dir inherited from the startup flags.
    let out = home.batch(
        &["-d", "/nonexistent"],
        &[
            init(),
            call(1, "project", json!({})),
            call(2, "project", json!({"all": true})),
        ],
    );
    assert_eq!(structured(&out, 1)["projects"], json!([]));
    assert_eq!(structured(&out, 2)["projects"].as_array().unwrap().len(), 2);
}

#[test]
fn memory_tool() {
    let home = Home::new();
    let project = home.project();
    let out = home.batch(
        &[],
        &[
            init(),
            call(1, "memory", json!({"dir": project})),
            call(
                2,
                "memory",
                json!({"dir": project, "fields": ["agent", "name", "body"]}),
            ),
            call(
                3,
                "memory",
                json!({"dir": project, "query": "oauth", "fields": ["name", "matched"]}),
            ),
            call(
                4,
                "memory",
                json!({"dir": project, "fields": ["name", "body"], "max_chars": 20}),
            ),
            call(5, "memory", json!({"dir": "/nonexistent"})),
            call(6, "memory", json!({"dir": project, "running": true})),
            call(
                7,
                "memory",
                json!({"dir": project, "agent": "claude", "fields": ["agent", "name"]}),
            ),
            call(8, "memory", json!({"dir": "/nonexistent", "query": "("})),
        ],
    );
    assert!(
        tool_error(&out, 8).contains("Invalid regex"),
        "an invalid query is an error even when no file exists"
    );
    let r1 = structured(&out, 1);
    assert_eq!(r1["scope"]["dir"], json!(project));
    let names: Vec<&str> = r1["memory"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"CLAUDE.md"), "{names:?}");
    assert!(names.contains(&"AGENTS.md"), "{names:?}");
    assert!(
        r1["memory"][0].get("body").is_none(),
        "body is not a default field"
    );
    assert_eq!(r1["truncated"], json!(false));

    let r2 = structured(&out, 2);
    let bodies: Vec<&str> = r2["memory"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert!(bodies.iter().any(|b| b.contains("Use tabs")));

    let r3 = structured(&out, 3);
    assert_eq!(r3["memory"].as_array().unwrap().len(), 1);
    assert_eq!(r3["memory"][0]["name"], json!("AGENTS.md"));
    assert!(
        r3["memory"][0]["matched"]
            .as_str()
            .unwrap()
            .contains("oauth")
    );

    let r4 = structured(&out, 4);
    assert_eq!(r4["truncated"], json!(true));
    let with_body = r4["memory"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["body"].is_string())
        .count();
    assert!(
        with_body < 2,
        "bodies are dropped from the end until the total fits"
    );
    assert_eq!(
        r4["memory"].as_array().unwrap().len(),
        2,
        "every file stays listed"
    );

    assert_eq!(structured(&out, 5)["memory"], json!([]));
    assert!(tool_error(&out, 6).contains("running"));
    for m in structured(&out, 7)["memory"].as_array().unwrap() {
        let agent = m["agent"].as_str().unwrap();
        assert!(agent == "claude" || agent == "shared", "{agent}");
    }
}

#[test]
fn agent_and_list_agents_tools() {
    let home = Home::new();
    let out = home.batch(
        &[],
        &[
            init(),
            call(1, "agent", json!({"all": true})),
            call(2, "agent", json!({"dir": SESSION_CWD})),
            call(3, "agent", json!({"dir": "/nonexistent"})),
            call(4, "agent", json!({"all": true, "agent": "codex"})),
            call(7, "agent", json!({"all": true, "project": "jp"})),
            call(8, "agent", json!({"all": true, "project": "nothing"})),
            call(5, "list_agents", json!({})),
            call(6, "list_agents", json!({"all": true})),
        ],
    );
    let r1 = structured(&out, 1);
    assert_eq!(r1["scope"]["all"], json!(true));
    assert_eq!(r1["agents"].as_array().unwrap().len(), 1);
    assert_eq!(r1["agents"][0]["agent"], json!("claude"));
    assert_eq!(r1["agents"][0]["sessions"], json!(2));
    assert!(r1["agents"][0]["latest"].is_string());
    assert!(r1["agents"][0].get("scope_sessions").is_none());

    let r2 = structured(&out, 2);
    assert_eq!(r2["scope"]["dir"], json!(SESSION_CWD));
    assert_eq!(r2["agents"][0]["sessions"], json!(2));
    assert_eq!(r2["agents"][0]["scope_sessions"], json!(1));
    assert!(r2["agents"][0]["scope_latest"].is_string());

    let r3 = structured(&out, 3);
    assert_eq!(
        r3["agents"][0]["scope_sessions"],
        json!(0),
        "zero rows are kept"
    );
    assert_eq!(structured(&out, 4)["agents"], json!([]));
    assert_eq!(structured(&out, 7)["agents"][0]["sessions"], json!(1));
    assert_eq!(structured(&out, 8)["agents"], json!([]));

    let r5 = structured(&out, 5);
    assert!(r5.get("scope").is_none());
    let agents = r5["agents"].as_array().unwrap();
    let claude = agents.iter().find(|a| a["id"] == json!("claude")).unwrap();
    assert_eq!(claude["builtin"], json!(true));
    assert_eq!(claude["disabled"], json!(false));
    assert!(
        claude["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("search"))
    );
    for id in [
        "codex", "gemini", "copilot", "cursor", "agy", "grok", "opencode",
    ] {
        assert!(agents.iter().any(|a| a["id"] == json!(id)), "{id}");
    }
    assert!(tool_error(&out, 6).contains("unknown argument"));
}

// ─── bidirectional: cancellation and panics ──────────────────────────

/// A running server whose stdout is read on a thread.
struct Live {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<Value>,
}

impl Live {
    fn spawn(home: &Home, envs: &[(&str, &str)]) -> Self {
        let mut cmd = home.command(&[]);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line).expect("JSON line");
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{}", msg).unwrap();
    }

    fn recv(&self) -> Value {
        self.lines
            .recv_timeout(Duration::from_secs(60))
            .expect("response within 60s")
    }

    /// Receive until the response with `id` arrives; returns the ids seen.
    fn recv_until(&self, id: u64) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let v = self.recv();
            seen.push(v["id"].clone());
            if v["id"] == json!(id) {
                return seen;
            }
        }
    }

    fn finish(mut self) {
        drop(self.stdin);
        let status = self.child.wait().unwrap();
        assert!(status.success());
    }
}

fn cancelled(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": "test"}})
}

#[test]
fn cancel_running_and_queued_calls() {
    let home = Home::new();
    let mut live = Live::spawn(&home, &[("AH_MCP_TEST_CHECKPOINT_DELAY_MS", "300")]);
    live.send(&init());
    assert_eq!(live.recv()["id"], json!(0));

    // Running: the slow log is cancelled; ping (reader thread) answers at
    // once and the next call completes without a response for the first.
    live.send(&call(10, "log", json!({"all": true})));
    std::thread::sleep(Duration::from_millis(100));
    live.send(&cancelled(10));
    live.send(&json!({"jsonrpc": "2.0", "id": 11, "method": "ping"}));
    live.send(&call(12, "list_agents", json!({})));
    let seen = live.recv_until(12);
    assert!(
        !seen.contains(&json!(10)),
        "cancelled call got a response: {seen:?}"
    );
    assert!(seen.contains(&json!(11)));

    // Queued: the second call is removed before it starts.
    live.send(&call(20, "log", json!({"all": true})));
    live.send(&call(21, "log", json!({"all": true})));
    live.send(&cancelled(21));
    live.send(&call(22, "list_agents", json!({})));
    let seen = live.recv_until(22);
    assert!(seen.contains(&json!(20)));
    assert!(!seen.contains(&json!(21)), "{seen:?}");

    // Completed: a late cancellation is ignored and the server keeps working.
    live.send(&cancelled(20));
    live.send(&call(23, "list_agents", json!({})));
    assert_eq!(live.recv()["id"], json!(23));
    live.finish();
}

#[test]
fn panic_in_tool_is_reported_and_server_survives() {
    let home = Home::new();
    let mut live = Live::spawn(&home, &[("AH_MCP_TEST_PANIC", "1")]);
    live.send(&init());
    assert_eq!(live.recv()["id"], json!(0));
    live.send(&call(1, "log", json!({"panic": true})));
    let r = live.recv();
    assert_eq!(r["id"], json!(1));
    assert_eq!(r["result"]["isError"], json!(true));
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("internal error")
    );
    live.send(&call(2, "list_agents", json!({})));
    assert_eq!(live.recv()["result"]["isError"], json!(false));
    live.finish();
}

#[test]
fn eof_drains_queued_calls_before_exit() {
    let home = Home::new();
    let out = home.batch(
        &[],
        &[
            init(),
            call(1, "log", json!({"all": true})),
            call(2, "log", json!({"all": true})),
            call(3, "list_agents", json!({})),
        ],
    );
    for id in 1..=3 {
        structured(&out, id);
    }
}

#[test]
fn show_at_cuts_a_huge_selected_record_to_a_window() {
    let home = Home::new();
    let big_id = "abc12345-0000-0000-0000-000000000003";
    let big = format!("{}needle{}", "x".repeat(150_000), "y".repeat(150_000));
    fs::write(
        home.path().join(format!(".claude/projects/-tmp-proj/{big_id}.jsonl")),
        format!(
            "{{\"type\":\"system\",\"cwd\":\"/Users/test/big\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{big}\"}}]}}}}\n"
        ),
    )
    .unwrap();
    let out = home.batch(
        &[],
        &[
            init(),
            call(
                1,
                "search",
                json!({"dir": "/Users/test/big", "pattern": "needle"}),
            ),
        ],
    );
    let position = structured(&out, 1)["hits"][0]["position"]
        .as_str()
        .unwrap()
        .to_string();
    let out = home.batch(
        &[],
        &[
            init(),
            call(
                1,
                "show",
                json!({"session": big_id, "at": position, "context": 0}),
            ),
            call(
                2,
                "show",
                json!({"session": big_id, "at": position, "context": 0, "max_chars": 200}),
            ),
        ],
    );
    // The default limit applies even to a single huge record.
    let r1 = structured(&out, 1);
    assert_eq!(r1["truncated"], json!(true));
    let text = r1["records"][0]["text"].as_str().unwrap();
    assert_eq!(text.chars().count(), 100_000);
    let (s, e) = (
        r1["records"][0]["match_start"].as_u64().unwrap() as usize,
        r1["records"][0]["match_end"].as_u64().unwrap() as usize,
    );
    assert_eq!(&text[s..e], "needle");
    let r2 = structured(&out, 2);
    let text = r2["records"][0]["text"].as_str().unwrap();
    assert_eq!(text.chars().count(), 200);
    assert!(text.contains("needle"));
    let content = by_id(&out, 2)["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert!(
        content[1]["text"]
            .as_str()
            .unwrap()
            .contains("smaller context")
    );
    // The first block stays valid JSON Lines even when truncated.
    for line in content[0]["text"].as_str().unwrap().lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
}

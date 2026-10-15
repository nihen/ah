//! `ah mcp`: a Model Context Protocol server over stdio.
//!
//! One newline-delimited JSON-RPC message per line; stdout carries nothing
//! else. The server is dual-era: it answers the `initialize` handshake of
//! protocol revisions up to 2025-11-25 and the stateless, per-request
//! `_meta` style of 2026-07-28 (`server/discover`). Requests that need no
//! scanning (`initialize`, `ping`, `tools/list`, `server/discover`) are
//! answered on the reader thread; `tools/call` runs on one worker thread,
//! in order, and can be cancelled with `notifications/cancelled`.

mod args;
mod rpc;
mod tools;

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use serde_json::{Map, Value, json};

use crate::cli::{FilterArgs, InteractiveArgs};
use rpc::{Request, RpcError};

/// Legacy (handshake) protocol revisions, newest first.
const LEGACY_VERSIONS: [&str; 2] = ["2025-11-25", "2025-06-18"];
/// Modern (per-request metadata) protocol revisions, newest first.
const MODERN_VERSIONS: [&str; 1] = ["2026-07-28"];
const SERVER_NAME: &str = "ah";
/// Test hook: sleep this long at every scan checkpoint.
const TEST_DELAY_ENV: &str = "AH_MCP_TEST_CHECKPOINT_DELAY_MS";
/// Test hook: a tool argument that makes `tools/call` panic.
const TEST_PANIC_ENV: &str = "AH_MCP_TEST_PANIC";

struct Job {
    id: Value,
    name: String,
    arguments: Map<String, Value>,
    modern: bool,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
    closed: AtomicBool,
    /// Id of the `tools/call` the worker is executing.
    running: Mutex<Option<Value>>,
    out: Mutex<std::io::Stdout>,
}

impl Shared {
    fn write(&self, msg: &Value) {
        // serde_json never emits a raw newline inside a document.
        let line = serde_json::to_string(msg).unwrap_or_default();
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        // A closed pipe means the client is gone; nothing to report to.
        let _ = out.write_all(line.as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

/// Validate the startup flags and run the server until stdin closes.
pub fn serve(filter: FilterArgs, ia: &InteractiveArgs) -> Result<(), String> {
    if filter.query.is_some() || filter.prompt_only || filter.raw_search {
        return Err(
            "ah mcp: -q/--query, -p/--prompt-only and --raw-search are not accepted; \
             pass query/pattern as tool arguments instead"
                .into(),
        );
    }
    if !filter.remote.is_empty() || filter.all_remote {
        return Err(
            "ah mcp: --remote/-A are not supported; run ah mcp on the host that holds the sessions"
                .into(),
        );
    }
    if ia.interactive {
        return Err("ah mcp: -i/--interactive does not apply".into());
    }
    if filter.color {
        return Err("ah mcp: --color does not apply (stdout carries protocol messages)".into());
    }
    if let Some(ms) = std::env::var_os(TEST_DELAY_ENV) {
        if let Some(ms) = ms.to_str().and_then(|s| s.parse::<u64>().ok()) {
            crate::cancel::set_checkpoint_delay_ms(ms);
        }
    }
    run(filter);
    Ok(())
}

fn run(filter: FilterArgs) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        wake: Condvar::new(),
        closed: AtomicBool::new(false),
        running: Mutex::new(None),
        out: Mutex::new(std::io::stdout()),
    });
    let base = Arc::new(filter);
    let worker = {
        let shared = Arc::clone(&shared);
        let base = Arc::clone(&base);
        std::thread::spawn(move || worker_loop(&shared, &base))
    };

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        handle_line(&shared, &base, trimmed);
    }

    // EOF: stop accepting, let queued calls finish, then exit. `closed` is
    // set under the queue lock so the worker cannot miss the wakeup between
    // checking the flag and waiting.
    {
        let _queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        shared.closed.store(true, Ordering::SeqCst);
        shared.wake.notify_all();
    }
    let _ = worker.join();
}

fn handle_line(shared: &Shared, base: &FilterArgs, line: &str) {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            shared.write(&rpc::error(
                &Value::Null,
                &RpcError::new(rpc::PARSE_ERROR, format!("Parse error: {}", e)),
            ));
            return;
        }
    };
    if !value.is_object() {
        shared.write(&rpc::error(
            &Value::Null,
            &RpcError::new(
                rpc::INVALID_REQUEST,
                "Invalid Request: expected a JSON object",
            ),
        ));
        return;
    }
    let req: Request = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            shared.write(&rpc::error(
                &Value::Null,
                &RpcError::new(rpc::INVALID_REQUEST, format!("Invalid Request: {}", e)),
            ));
            return;
        }
    };
    if let Err(e) = req.validate() {
        // A malformed id cannot be echoed; such requests get a null id.
        let id = match &req.id {
            Some(id) if id.is_string() || id.is_number() => id.clone(),
            _ => Value::Null,
        };
        shared.write(&rpc::error(&id, &RpcError::new(rpc::INVALID_REQUEST, e)));
        return;
    }
    if crate::color::is_debug() {
        eprintln!(
            "[debug] mcp: {} id={}",
            req.method,
            req.id.as_ref().unwrap_or(&Value::Null)
        );
    }

    let Some(id) = req.id.clone() else {
        handle_notification(shared, &req);
        return;
    };

    // Modern requests name their protocol version per request.
    let modern = req.meta_protocol_version().is_some();
    if let Some(version) = req.meta_protocol_version() {
        if !MODERN_VERSIONS.contains(&version) {
            let mut err = RpcError::new(
                rpc::UNSUPPORTED_PROTOCOL_VERSION,
                "Unsupported protocol version",
            );
            err.data = Some(json!({ "supported": MODERN_VERSIONS, "requested": version }));
            shared.write(&rpc::error(&id, &err));
            return;
        }
        if !req.has_meta_client_capabilities() {
            shared.write(&rpc::error(
                &id,
                &RpcError::invalid_params(
                    "_meta is missing io.modelcontextprotocol/clientCapabilities",
                ),
            ));
            return;
        }
    }

    let result = match req.method.as_str() {
        "initialize" => Ok(initialize_result(base, req.params_map())),
        "server/discover" => Ok(discover_result(base)),
        "ping" => Ok(finish(json!({}), modern, false)),
        "tools/list" => Ok(finish(
            json!({ "tools": tools::definitions() }),
            modern,
            true,
        )),
        "tools/call" => match parse_call(&req) {
            Ok((name, arguments)) => {
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.push_back(Job {
                    id,
                    name,
                    arguments,
                    modern,
                });
                shared.wake.notify_one();
                return;
            }
            Err(e) => Err(e),
        },
        other => Err(RpcError::new(
            rpc::METHOD_NOT_FOUND,
            format!("Method not found: {}", other),
        )),
    };
    match result {
        Ok(result) => shared.write(&rpc::response(&id, result)),
        Err(err) => shared.write(&rpc::error(&id, &err)),
    }
}

fn handle_notification(shared: &Shared, req: &Request) {
    // `notifications/initialized` and anything else need no action.
    if req.method != "notifications/cancelled" {
        return;
    }
    let Some(target) = req.params.get("requestId") else {
        return;
    };
    // Hold the queue lock while inspecting `running` too: the worker moves
    // a job from the queue to `running` under the same lock, so a request
    // is always in exactly one of the two places (lock order: queue, running).
    let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = queue.iter().position(|j| j.id == *target) {
        queue.remove(pos);
        return;
    }
    let running = shared.running.lock().unwrap_or_else(|e| e.into_inner());
    if running.as_ref() == Some(target) {
        crate::cancel::request();
    }
    // Completed or unknown: ignored, as the protocol allows.
}

fn parse_call(req: &Request) -> Result<(String, Map<String, Value>), RpcError> {
    let params = req
        .params_map()
        .ok_or_else(|| RpcError::invalid_params("tools/call requires params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("tools/call requires a string 'name'"))?;
    if !tools::exists(name) {
        return Err(RpcError::invalid_params(format!("Unknown tool: {}", name)));
    }
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(RpcError::invalid_params("'arguments' must be an object")),
    };
    Ok((name.to_string(), arguments))
}

fn server_info() -> Value {
    json!({ "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") })
}

/// Add the fields 2026-07-28 results carry (`resultType`, server identity,
/// cache hints for list results). Legacy clients get the bare result.
fn finish(mut result: Value, modern: bool, cacheable: bool) -> Value {
    if modern {
        if let Some(map) = result.as_object_mut() {
            map.insert("resultType".into(), "complete".into());
            map.insert(
                "_meta".into(),
                json!({ "io.modelcontextprotocol/serverInfo": server_info() }),
            );
            if cacheable {
                map.insert("ttlMs".into(), json!(3_600_000));
                map.insert("cacheScope".into(), json!("private"));
            }
        }
    }
    result
}

fn initialize_result(base: &FilterArgs, params: Option<&Map<String, Value>>) -> Value {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = if LEGACY_VERSIONS.contains(&requested) {
        requested
    } else {
        LEGACY_VERSIONS[0]
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": server_info(),
        "instructions": instructions(base),
    })
}

fn discover_result(base: &FilterArgs) -> Value {
    let mut result = json!({
        "supportedVersions": MODERN_VERSIONS,
        "capabilities": { "tools": { "listChanged": false } },
        "instructions": instructions(base),
    });
    result = finish(result, true, true);
    result
}

fn instructions(base: &FilterArgs) -> String {
    let scope = if base.all {
        "every project directory".to_string()
    } else {
        format!(
            "sessions whose working directory is {}",
            FilterArgs::resolve_dir(base.dir.as_deref().unwrap_or("."))
        )
    };
    format!(
        "ah searches past coding-agent sessions (Claude Code, Codex, Gemini CLI, Copilot, Cursor, Grok, opencode, ...) on this host. \
         By default tools cover {scope}; pass all: true or dir to change the scope. \
         query and pattern are case-insensitive regular expressions. \
         Use search to find passages, then show with at (and context) to read the records around a hit; \
         use show with format md for a whole transcript. \
         Results of log, search, project, memory and agent include scope ({{dir, all}}) so an empty result can be told from a wrong scope."
    )
}

fn worker_loop(shared: &Shared, base: &FilterArgs) {
    loop {
        let job = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = queue.pop_front() {
                    // Register the job as running before releasing the queue
                    // lock, so a cancellation arriving now still finds it.
                    let mut running = shared.running.lock().unwrap_or_else(|e| e.into_inner());
                    *running = Some(job.id.clone());
                    crate::cancel::reset();
                    break job;
                }
                if shared.closed.load(Ordering::SeqCst) {
                    return;
                }
                queue = shared.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        crate::agents::clear_all_caches();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            if std::env::var_os(TEST_PANIC_ENV).is_some() && job.arguments.contains_key("panic") {
                panic!("test panic");
            }
            tools::call(&job.name, &job.arguments, base)
        }));
        let cancelled = {
            let mut running = shared.running.lock().unwrap_or_else(|e| e.into_inner());
            *running = None;
            crate::cancel::requested()
        };
        if cancelled {
            // The protocol asks for no response to a cancelled request.
            continue;
        }
        let msg = match outcome {
            Ok(Ok(Ok(output))) => rpc::response(
                &job.id,
                finish(
                    json!({
                        "content": [{ "type": "text", "text": output.text }],
                        "structuredContent": output.structured,
                        "isError": false,
                    }),
                    job.modern,
                    false,
                ),
            ),
            Ok(Ok(Err(message))) => rpc::response(
                &job.id,
                finish(
                    json!({
                        "content": [{ "type": "text", "text": message }],
                        "isError": true,
                    }),
                    job.modern,
                    false,
                ),
            ),
            Ok(Err(rpc_err)) => rpc::error(&job.id, &rpc_err),
            Err(panic) => {
                let detail = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".into());
                rpc::response(
                    &job.id,
                    finish(
                        json!({
                            "content": [{ "type": "text", "text": format!("internal error: {}", detail) }],
                            "isError": true,
                        }),
                        job.modern,
                        false,
                    ),
                )
            }
        };
        shared.write(&msg);
    }
}

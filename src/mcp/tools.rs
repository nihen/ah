//! MCP tool definitions and dispatch. Every tool reuses the CLI's data
//! builders; nothing here prints.
use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::args::{self, Params, common_schema, merge_filter};
use super::rpc::RpcError;
use crate::agents::common::canonical_home;
use crate::agents::entries::SearchKind;
use crate::cli::{
    Field, FilterArgs, ListProjectsResolvedArgs, MatchSearchArgs, MemoryField, MemoryResolvedArgs,
    OutputFormat, ProjectField, SortOrder,
};
use crate::{
    agent_stats, agents, collector, list_agents, memory, pipeline, projects, resolver, search_hits,
    show, subcmd,
};

pub const DEFAULT_MAX_RESULTS: u64 = 50;
pub const DEFAULT_MAX_CHARS: u64 = 100_000;

/// A successful tool execution: the text block and the structured value.
pub struct ToolOutput {
    pub text: String,
    pub structured: Value,
}

impl ToolOutput {
    /// Pretty JSON as the text block, the same value as structured content.
    fn json(structured: Value) -> Self {
        let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
        Self { text, structured }
    }
}

pub const TOOL_NAMES: [&str; 7] = [
    "log",
    "search",
    "show",
    "project",
    "memory",
    "agent",
    "list_agents",
];

pub fn exists(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

const LOG_COMMON: [&str; 10] = [
    args::ALL,
    args::DIR,
    args::AGENT,
    args::PROJECT,
    args::SINCE,
    args::UNTIL,
    args::RUNNING,
    args::NO_ARCHIVED,
    args::SUBAGENTS,
    args::LIMIT,
];
const PROJECT_COMMON: [&str; 8] = [
    args::DIR,
    args::AGENT,
    args::PROJECT,
    args::SINCE,
    args::UNTIL,
    args::NO_ARCHIVED,
    args::SUBAGENTS,
    args::LIMIT,
];
const MEMORY_COMMON: [&str; 6] = [
    args::ALL,
    args::DIR,
    args::AGENT,
    args::PROJECT,
    args::SINCE,
    args::UNTIL,
];
const AGENT_COMMON: [&str; 8] = [
    args::ALL,
    args::DIR,
    args::AGENT,
    args::SINCE,
    args::UNTIL,
    args::NO_ARCHIVED,
    args::SUBAGENTS,
    args::LIMIT,
];

fn schema(common: &[&str], extra: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let mut props = Map::new();
    for name in common {
        props.insert((*name).to_string(), common_schema(name));
    }
    for (name, def) in extra {
        props.insert(name.to_string(), def);
    }
    let mut s = json!({
        "type": "object",
        "properties": Value::Object(props),
        "additionalProperties": false,
    });
    if !required.is_empty() {
        s["required"] = json!(required);
    }
    s
}

fn names<T>(all: Vec<T>, name: impl Fn(&T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(name).collect()
}

/// The `tools/list` entries, in a fixed order.
pub fn definitions() -> Vec<Value> {
    let field_names = names(Field::all(), Field::name);
    let project_fields = names(ProjectField::all(), ProjectField::name);
    let memory_fields = names(MemoryField::all(), MemoryField::name);
    let order = json!({"type": "string", "enum": ["asc", "desc"], "description": "Sort direction (default desc)"});
    vec![
        json!({
            "name": "log",
            "title": "List sessions",
            "description": "List coding-agent sessions (Claude Code, Codex, Gemini CLI, Copilot, Cursor, Grok, opencode, ...) newest first, optionally matching a regex over the conversation. Returns metadata per session; use show to read a transcript.",
            "inputSchema": schema(&LOG_COMMON, vec![
                ("query", json!({"type": "string", "description": "Case-insensitive regex over user/assistant messages and tool input/output"})),
                ("prompt_only", json!({"type": "boolean", "description": "Match query against user prompts only"})),
                ("raw_search", json!({"type": "boolean", "description": "Match query against the raw session file bytes, including metadata"})),
                ("fields", json!({"type": "array", "items": {"type": "string", "enum": field_names}, "description": "Fields to return per session (default: agent, project, modified_at, title, id, plus matched when query is set)"})),
                ("sort", json!({"type": "string", "enum": field_names, "description": "Sort field (default modified_at)"})),
                ("order", order.clone()),
                ("max_results", json!({"type": "integer", "minimum": 0, "description": "Return at most this many sessions (default 50, 0 = unlimited)"})),
                ("transcript_limit", json!({"type": "integer", "minimum": 0, "description": "Max characters of the transcript field (default 500, 0 = unlimited)"})),
                ("title_limit", json!({"type": "integer", "minimum": 0, "description": "Max characters of auto-generated titles (default 50, 0 = unlimited)"})),
            ], &[]),
        }),
        json!({
            "name": "search",
            "title": "Search passages",
            "description": "Find every occurrence of a regex in session conversations and tool input/output. Each hit has a snippet and a position; pass the position to show (at) to read the surrounding records.",
            "inputSchema": schema(&LOG_COMMON, vec![
                ("pattern", json!({"type": "string", "description": "Case-insensitive regex"})),
                ("kind", json!({"type": "array", "items": {"type": "string", "enum": ["user", "assistant", "tool-input", "tool-output", "unknown"]}, "description": "Only these record kinds (default: all)"})),
                ("prompt_only", json!({"type": "boolean", "description": "Only user prompts (same as kind: [\"user\"])"})),
                ("max_matches", json!({"type": "integer", "minimum": 0, "description": "Stop after this many occurrences (default 100, 0 = unlimited)"})),
                ("snippet_length", json!({"type": "integer", "minimum": 1, "description": "Max characters per snippet including ellipses (default 240)"})),
            ], &["pattern"]),
        }),
        json!({
            "name": "show",
            "title": "Show transcript",
            "description": "Read one session: the whole conversation as Markdown or JSON messages, or, with at, the source records around a search hit (tool input/output included).",
            "inputSchema": schema(&[], vec![
                ("session", json!({"type": "string", "description": "Session id (or unique prefix) or session file path, as returned by log/search"})),
                ("format", json!({"type": "string", "enum": ["md", "json"], "description": "md (default): Markdown transcript; json: [{role, text}] messages"})),
                ("head", json!({"type": "integer", "minimum": 0, "description": "Only the first N messages"})),
                ("at", json!({"type": "string", "description": "A position from search; returns the records around it instead of the transcript"})),
                ("context", json!({"type": "integer", "minimum": 0, "maximum": 1000, "description": "Records before and after the position (default 2)"})),
                ("max_chars", json!({"type": "integer", "minimum": 0, "description": "Truncate the result to this many characters (default 100000, 0 = unlimited)"})),
            ], &["session"]),
        }),
        json!({
            "name": "project",
            "title": "List projects",
            "description": "List known projects (working directories with sessions) with session counts and agents. Covers every directory unless dir is given.",
            "inputSchema": schema(&PROJECT_COMMON, vec![
                ("fields", json!({"type": "array", "items": {"type": "string", "enum": project_fields.clone()}, "description": "Fields per project (default: project, session_count, last_modified_at, agents)"})),
                ("sort", json!({"type": "string", "enum": project_fields, "description": "Sort field (default last_modified_at)"})),
                ("order", order.clone()),
            ], &[]),
        }),
        json!({
            "name": "memory",
            "title": "List memory files",
            "description": "List agent memory and instruction files (CLAUDE.md, AGENTS.md, rules, auto memory, skills) for the current project, or every project with all. Request the body field to get file contents.",
            "inputSchema": schema(&MEMORY_COMMON, vec![
                ("fields", json!({"type": "array", "items": {"type": "string", "enum": memory_fields.clone()}, "description": "Fields per file (default: agent, project, type, name, modified_at, description); add body for contents"})),
                ("type", json!({"type": "string", "description": "Only this memory type (e.g. instruction, rule, memory, skill; skills are listed only when requested)"})),
                ("query", json!({"type": "string", "description": "Case-insensitive regex over file contents; the matched field holds the hit"})),
                ("sort", json!({"type": "string", "enum": memory_fields, "description": "Sort field (default modified_at)"})),
                ("order", order.clone()),
                ("max_chars", json!({"type": "integer", "minimum": 0, "description": "Drop body contents from the end once the total exceeds this (default 100000, 0 = unlimited)"})),
            ], &[]),
        }),
        json!({
            "name": "agent",
            "title": "Sessions per agent",
            "description": "Session count and latest activity per agent, overall and within the directory scope.",
            "inputSchema": schema(&AGENT_COMMON, vec![], &[]),
        }),
        json!({
            "name": "list_agents",
            "title": "Supported agents",
            "description": "The agents this server can read, with their session file patterns and capabilities.",
            "inputSchema": json!({"type": "object", "additionalProperties": false}),
        }),
    ]
}

/// Run `name` with `arguments`. `Err` is a protocol error (unknown tool);
/// tool execution errors are `Ok(Err(message))`.
pub fn call(
    name: &str,
    arguments: &Map<String, Value>,
    base: &FilterArgs,
) -> Result<Result<ToolOutput, String>, RpcError> {
    let p = Params::new(arguments);
    Ok(match name {
        "log" => tool_log(&p, base),
        "search" => tool_search(&p, base),
        "show" => tool_show(&p),
        "project" => tool_project(&p, base),
        "memory" => tool_memory(&p, base),
        "agent" => tool_agent(&p, base),
        "list_agents" => tool_list_agents(&p),
        _ => return Err(RpcError::invalid_params(format!("Unknown tool: {}", name))),
    })
}

/// Apply the per-call listing flags the collector reads from process state.
fn apply_collector_flags(filter: &FilterArgs) {
    collector::init_exclude_archived(filter.no_archived);
    collector::init_include_subagents(filter.subagents);
}

fn empty_is_ok<T>(r: Result<Vec<T>, String>) -> Result<Vec<T>, String> {
    match r {
        Err(e) if crate::is_empty_result_error(&e) => Ok(Vec::new()),
        other => other,
    }
}

fn sort_order(p: &Params<'_>) -> Result<SortOrder, String> {
    match p.string("order")?.as_deref() {
        None | Some("desc") => Ok(SortOrder::Desc),
        Some("asc") => Ok(SortOrder::Asc),
        Some(other) => Err(format!("'order' must be asc or desc, got '{}'", other)),
    }
}

fn parse_fields<T: std::str::FromStr<Err = String>>(
    p: &Params<'_>,
    key: &str,
) -> Result<Option<Vec<T>>, String> {
    let Some(list) = p.string_list(key)? else {
        return Ok(None);
    };
    if list.is_empty() {
        return Err(format!("'{}' must list at least one field", key));
    }
    list.iter()
        .map(|s| s.parse::<T>())
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn with_common<'a>(common: &[&'a str], extra: &[&'a str]) -> Vec<&'a str> {
    common.iter().chain(extra.iter()).copied().collect()
}

// ─── log ──────────────────────────────────────────────────────────────

fn tool_log(p: &Params<'_>, base: &FilterArgs) -> Result<ToolOutput, String> {
    p.check_allowed(&with_common(
        &LOG_COMMON,
        &[
            "query",
            "prompt_only",
            "raw_search",
            "fields",
            "sort",
            "order",
            "max_results",
            "transcript_limit",
            "title_limit",
        ],
    ))?;
    let mut filter = merge_filter(base, p, &LOG_COMMON)?;
    filter.query = p.string("query")?.filter(|q| !q.is_empty());
    filter.prompt_only = p.bool("prompt_only")?.unwrap_or(false);
    filter.raw_search = p.bool("raw_search")?.unwrap_or(false);
    if filter.prompt_only && filter.raw_search {
        return Err("'prompt_only' and 'raw_search' are exclusive".into());
    }
    let query = filter.query.clone().unwrap_or_default();
    let sort_field = match p.string("sort")? {
        Some(s) => s.parse::<Field>()?,
        None => Field::ModifiedAt,
    };
    let fields = match parse_fields::<Field>(p, "fields")? {
        Some(f) => f,
        None if query.is_empty() => vec![
            Field::Agent,
            Field::Project,
            Field::ModifiedAt,
            Field::Title,
            Field::Id,
        ],
        None => vec![
            Field::Agent,
            Field::Project,
            Field::ModifiedAt,
            Field::Title,
            Field::Matched,
            Field::Id,
        ],
    };
    let max_results = p.uint("max_results")?.unwrap_or(DEFAULT_MAX_RESULTS) as usize;
    let transcript_limit = p.uint("transcript_limit")?.unwrap_or(500) as usize;
    let title_limit = p.uint("title_limit")?.unwrap_or(50) as usize;

    apply_collector_flags(&filter);
    let search_mode = filter.search_mode();
    let params = pipeline::PipelineParams {
        resolve_fields: fields.clone(),
        resolve_opts: resolver::ResolveOpts::new(&query, transcript_limit, title_limit)
            .with_search_mode(search_mode.clone()),
        filters: filter.to_filters(),
        since: filter.since_time()?,
        until: filter.until_time()?,
        query,
        search_mode,
        sort_field,
        sort_order: sort_order(p)?,
        collect_limit: filter.limit,
        running: filter.running,
        require_resume_cmd: false,
    };
    let mut sessions = pipeline::run_pipeline(&params)?.sessions;
    let total = sessions.len();
    let truncated = max_results != 0 && total > max_results;
    if truncated {
        sessions.truncate(max_results);
    }
    let rows: Vec<Value> = sessions
        .iter()
        .map(|s| Value::Object(crate::output::session_to_json(s, &fields)))
        .collect();
    Ok(ToolOutput::json(json!({
        "scope": args::scope_json(&filter),
        "sessions": rows,
        "total": total,
        "truncated": truncated,
    })))
}

// ─── search ───────────────────────────────────────────────────────────

fn tool_search(p: &Params<'_>, base: &FilterArgs) -> Result<ToolOutput, String> {
    p.check_allowed(&with_common(
        &LOG_COMMON,
        &[
            "pattern",
            "kind",
            "prompt_only",
            "max_matches",
            "snippet_length",
        ],
    ))?;
    let mut filter = merge_filter(base, p, &LOG_COMMON)?;
    let pattern = p.required_string("pattern")?;
    let mut kinds: Vec<SearchKind> = match p.string_list("kind")? {
        Some(list) => list
            .iter()
            .map(|k| {
                serde_json::from_value::<SearchKind>(Value::String(k.clone())).map_err(|_| {
                    format!(
                        "unknown kind '{}'; expected user, assistant, tool-input, tool-output or unknown",
                        k
                    )
                })
            })
            .collect::<Result<_, _>>()?,
        None => Vec::new(),
    };
    if p.bool("prompt_only")?.unwrap_or(false) {
        if kinds.iter().any(|k| *k != SearchKind::User) {
            return Err(
                "'prompt_only' selects user text; do not combine it with other kinds".into(),
            );
        }
        kinds = vec![SearchKind::User];
    }
    let max_matches = p.uint("max_matches")?.unwrap_or(100) as usize;
    let snippet_length = match p.uint("snippet_length")?.unwrap_or(240) {
        0 => return Err("'snippet_length' must be at least 1".into()),
        n => n.min(u32::MAX as u64) as u32,
    };
    let search_args = MatchSearchArgs {
        kind: kinds,
        pattern: Some(pattern),
        json: true,
        tsv: false,
        verbose: false,
        search_wire: false,
        max_matches,
        snippet_length,
    };
    apply_collector_flags(&filter);
    let (hits, truncated) = search_hits::collect_hits(&search_args, &mut filter)?;
    Ok(ToolOutput::json(json!({
        "scope": args::scope_json(&filter),
        "hits": hits,
        "truncated": truncated,
    })))
}

// ─── show ─────────────────────────────────────────────────────────────

/// Cut `s` to at most `max` characters on a character boundary.
pub fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if max == 0 {
        return (s.to_string(), false);
    }
    match s.char_indices().nth(max) {
        Some((idx, _)) => (s[..idx].to_string(), true),
        None => (s.to_string(), false),
    }
}

/// Drop trailing items until the summed character count of `len(item)`
/// fits in `max` (0 = unlimited). Returns whether anything was dropped.
fn fit_chars<T>(items: &mut Vec<T>, max: usize, len: impl Fn(&T) -> usize) -> bool {
    if max == 0 {
        return false;
    }
    let mut total: usize = items.iter().map(&len).sum();
    let mut dropped = false;
    while total > max && !items.is_empty() {
        let last = items.pop().unwrap();
        total -= len(&last);
        dropped = true;
    }
    dropped
}

fn truncation_note(what: &str) -> String {
    format!(
        "\n\n[truncated: {} exceeded max_chars; use head or a larger max_chars]",
        what
    )
}

fn tool_show(p: &Params<'_>) -> Result<ToolOutput, String> {
    p.check_allowed(&["session", "format", "head", "at", "context", "max_chars"])?;
    let session = p.required_string("session")?;
    let format = p.string("format")?.unwrap_or_else(|| "md".into());
    if format != "md" && format != "json" {
        return Err(format!("'format' must be md or json, got '{}'", format));
    }
    let head = p.uint("head")?.map(|n| n as usize).filter(|n| *n > 0);
    let at = p.string("at")?.filter(|s| !s.is_empty());
    let context = p.uint("context")?;
    if context.is_some() && at.is_none() {
        return Err("'context' requires 'at'".into());
    }
    if context.is_some_and(|c| c > 1000) {
        return Err("'context' must be at most 1000".into());
    }
    if at.is_some() && head.is_some() {
        return Err("'head' does not apply with 'at'".into());
    }
    let max_chars = p.uint("max_chars")?.unwrap_or(DEFAULT_MAX_CHARS) as usize;

    let home = canonical_home();
    // Explicit references reach archived and subagent sessions too.
    collector::init_exclude_archived(false);
    collector::init_include_subagents(true);
    let path = subcmd::resolve_session_ref(&session, &home)?;
    let plugin = agents::find_plugin_for_path(&path);
    let mtime = plugin
        .session_mtime(&path)
        .ok_or_else(|| format!("Failed to read session metadata: {}", path.display()))?;
    let meta_fields = [Field::Agent, Field::Id, Field::Title];
    let meta = resolver::resolve_fields(
        &path,
        plugin,
        mtime,
        &home,
        &meta_fields,
        &resolver::ResolveOpts::default_with_title_limit(50),
    );
    let field = |f: Field| meta.get(&f).cloned().unwrap_or_default();
    let session_json = json!({
        "path": path.to_string_lossy(),
        "agent": field(Field::Agent),
        "id": field(Field::Id),
        "title": field(Field::Title),
    });

    if let Some(at) = at {
        let mut records = show::collect_at_records(&path, &at, context.unwrap_or(2) as usize)?;
        let truncated = fit_chars(&mut records, max_chars, |r| r.text.chars().count());
        let structured = json!({
            "session": session_json,
            "records": records,
            "truncated": truncated,
        });
        let mut text = serde_json::to_string_pretty(&structured).unwrap_or_default();
        if truncated {
            text.push_str(&truncation_note("records"));
        }
        return Ok(ToolOutput { text, structured });
    }

    if format == "json" {
        let mut messages = show::collect_messages(&path, head);
        let truncated = fit_chars(&mut messages, max_chars, |(_, t)| t.chars().count());
        let messages: Vec<Value> = messages
            .into_iter()
            .map(|(role, text)| json!({ "role": role, "text": text }))
            .collect();
        let structured = json!({
            "session": session_json,
            "messages": messages,
            "truncated": truncated,
        });
        let mut text = serde_json::to_string_pretty(&structured).unwrap_or_default();
        if truncated {
            text.push_str(&truncation_note("messages"));
        }
        return Ok(ToolOutput { text, structured });
    }

    let markdown = show::render_md(&path, head);
    let (markdown, truncated) = truncate_chars(&markdown, max_chars);
    let mut text = markdown.clone();
    if truncated {
        text.push_str(&truncation_note("transcript"));
    }
    Ok(ToolOutput {
        text,
        structured: json!({
            "session": session_json,
            "markdown": markdown,
            "truncated": truncated,
        }),
    })
}

// ─── project ──────────────────────────────────────────────────────────

fn tool_project(p: &Params<'_>, base: &FilterArgs) -> Result<ToolOutput, String> {
    p.check_allowed(&with_common(&PROJECT_COMMON, &["fields", "sort", "order"]))?;
    let filter = merge_filter(base, p, &PROJECT_COMMON)?;
    let fields = parse_fields::<ProjectField>(p, "fields")?.unwrap_or_else(|| {
        vec![
            ProjectField::Project,
            ProjectField::SessionCount,
            ProjectField::LastModifiedAt,
            ProjectField::Agents,
        ]
    });
    let sort_field = match p.string("sort")? {
        Some(s) => s.parse::<ProjectField>()?,
        None => ProjectField::LastModifiedAt,
    };
    let resolved = ListProjectsResolvedArgs {
        fields: fields.clone(),
        output_format: OutputFormat::Json,
        sort_order: sort_order(p)?,
        sort_field,
    };
    apply_collector_flags(&filter);
    let records = empty_is_ok(projects::build_project_records(&resolved, &filter))?;
    let rows: Vec<Value> = records
        .iter()
        .map(|r| Value::Object(crate::output::project_to_json(r, &fields)))
        .collect();
    Ok(ToolOutput::json(json!({
        "scope": args::dir_only_scope_json(&filter),
        "projects": rows,
    })))
}

// ─── memory ───────────────────────────────────────────────────────────

fn tool_memory(p: &Params<'_>, base: &FilterArgs) -> Result<ToolOutput, String> {
    p.check_allowed(&with_common(
        &MEMORY_COMMON,
        &["fields", "type", "query", "sort", "order", "max_chars"],
    ))?;
    let mut filter = merge_filter(base, p, &MEMORY_COMMON)?;
    filter.query = p.string("query")?.filter(|q| !q.is_empty());
    let fields = parse_fields::<MemoryField>(p, "fields")?.unwrap_or_else(|| {
        vec![
            MemoryField::Agent,
            MemoryField::Project,
            MemoryField::Type,
            MemoryField::Name,
            MemoryField::ModifiedAt,
            MemoryField::Description,
        ]
    });
    let sort_field = match p.string("sort")? {
        Some(s) => s.parse::<MemoryField>()?,
        None => MemoryField::ModifiedAt,
    };
    let max_chars = p.uint("max_chars")?.unwrap_or(DEFAULT_MAX_CHARS) as usize;
    let resolved = MemoryResolvedArgs {
        fields: fields.clone(),
        output_format: OutputFormat::Json,
        sort_order: sort_order(p)?,
        sort_field,
        memory_type: p.string("type")?.filter(|t| !t.is_empty()),
    };
    let records: Vec<BTreeMap<MemoryField, String>> =
        empty_is_ok(memory::build_memory_records(&resolved, &filter))?;
    let mut rows: Vec<Map<String, Value>> = records
        .iter()
        .map(|r| crate::output::memory_to_json(r, &fields))
        .collect();
    // Bodies are the only large values: drop them from the end until the
    // total fits, keeping every file listed.
    let mut truncated = false;
    if fields.contains(&MemoryField::Body) && max_chars != 0 {
        let body_len = |row: &Map<String, Value>| {
            row.get("body")
                .and_then(Value::as_str)
                .map(|s| s.chars().count())
                .unwrap_or(0)
        };
        let mut total: usize = rows.iter().map(body_len).sum();
        for row in rows.iter_mut().rev() {
            if total <= max_chars {
                break;
            }
            let len = body_len(row);
            if len > 0 {
                row.insert("body".into(), Value::Null);
                total -= len;
                truncated = true;
            }
        }
    }
    let rows: Vec<Value> = rows.into_iter().map(Value::Object).collect();
    Ok(ToolOutput::json(json!({
        "scope": args::scope_json(&filter),
        "memory": rows,
        "truncated": truncated,
    })))
}

// ─── agent ────────────────────────────────────────────────────────────

fn tool_agent(p: &Params<'_>, base: &FilterArgs) -> Result<ToolOutput, String> {
    p.check_allowed(&AGENT_COMMON)?;
    let filter = merge_filter(base, p, &AGENT_COMMON)?;
    apply_collector_flags(&filter);
    let scope = args::scope_json(&filter);
    let scope_dir = scope.get("dir").and_then(Value::as_str).map(str::to_string);
    let stats = empty_is_ok(agent_stats::build_agent_stats(
        &filter,
        scope_dir.as_deref(),
    ))?;
    Ok(ToolOutput::json(json!({
        "scope": scope,
        "agents": stats,
    })))
}

// ─── list_agents ──────────────────────────────────────────────────────

fn tool_list_agents(p: &Params<'_>) -> Result<ToolOutput, String> {
    p.check_allowed(&[])?;
    Ok(ToolOutput::json(json!({
        "agents": list_agents::agent_defs_to_json(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_respects_boundaries() {
        assert_eq!(
            truncate_chars("日本語テキスト", 3),
            ("日本語".to_string(), true)
        );
        assert_eq!(truncate_chars("abc", 3), ("abc".to_string(), false));
        assert_eq!(truncate_chars("abc", 0), ("abc".to_string(), false));
    }

    #[test]
    fn fit_chars_drops_from_the_end() {
        let mut v = vec!["aaaa".to_string(), "bb".to_string(), "c".to_string()];
        assert!(fit_chars(&mut v, 6, |s| s.len()));
        assert_eq!(v, vec!["aaaa".to_string(), "bb".to_string()]);
        assert!(!fit_chars(&mut v, 0, |s| s.len()));
    }

    #[test]
    fn definitions_cover_every_tool() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        for d in &defs {
            assert_eq!(d["inputSchema"]["additionalProperties"], Value::Bool(false));
        }
    }
}

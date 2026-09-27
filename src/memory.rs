//! Memory and instruction file listing and search (shared by `ah memory`).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use regex::Regex;

use crate::agents;
use crate::agents::common::{canonical_home, format_mtime};
use crate::agents::{AgentPlugin, MemoryKind, MemorySource};
use crate::cli::{Field, FilterArgs, MemoryField, MemoryResolvedArgs, SortOrder};
use crate::collector;
use crate::config;
use crate::output;
use crate::resolver;

/// A collected memory/instruction entry before field resolution.
struct MemoryEntry {
    path: PathBuf,
    mtime: SystemTime,
    ctime: SystemTime,
    size: u64,
    agent: &'static str,
    project: String,
    memory_type: String,
    name: String,
    description: String,
    body: String,
}

struct MemoryFrontmatter {
    name: String,
    description: String,
    memory_type: String,
}

/// Parse YAML frontmatter from a memory file.
/// Returns (frontmatter, body) where body is the content after the closing `---`.
fn parse_frontmatter(content: &str) -> (MemoryFrontmatter, String) {
    let mut fm = MemoryFrontmatter {
        name: String::new(),
        description: String::new(),
        memory_type: String::new(),
    };

    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return (fm, content.to_string());
    }

    let after_first = if let Some(s) = content.strip_prefix("---\r\n") {
        s
    } else if let Some(s) = content.strip_prefix("---\n") {
        s
    } else {
        return (fm, content.to_string());
    };

    // Find closing delimiter: must be a standalone "---" line
    let mut end_opt: Option<usize> = None;
    let mut body_start: usize = 0;
    let mut offset: usize = 0;
    for chunk in after_first.split_inclusive('\n') {
        let line = chunk.trim_end_matches(['\n', '\r']);
        if line == "---" {
            end_opt = Some(offset);
            body_start = offset + chunk.len();
            break;
        }
        offset += chunk.len();
    }
    // Handle final line without trailing newline
    if end_opt.is_none() && !after_first.ends_with('\n') && offset < after_first.len() {
        let line = after_first[offset..].trim_end_matches('\r');
        if line == "---" {
            end_opt = Some(offset);
            body_start = after_first.len();
        }
    }

    let Some(end) = end_opt else {
        return (fm, content.to_string());
    };

    let fm_text = &after_first[..end];
    let body = after_first[body_start..]
        .trim_start_matches("\r\n")
        .trim_start_matches('\n')
        .to_string();

    for line in fm_text.lines() {
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "name" => fm.name = unquote_scalar(val),
                "description" => fm.description = unquote_scalar(val),
                "type" => fm.memory_type = unquote_scalar(val),
                _ => {}
            }
        }
    }

    (fm, body)
}

/// Strip one pair of surrounding YAML quotes from a frontmatter scalar.
/// Double-quoted values unescape `\"` and `\\`; single-quoted values
/// unescape `''`. Unquoted or unbalanced values are returned as-is.
fn unquote_scalar(val: &str) -> String {
    if val.len() >= 2 {
        // The closing quote must not itself be escaped (odd backslash run).
        let closing_escaped =
            |inner: &str| inner.chars().rev().take_while(|&c| c == '\\').count() % 2 == 1;
        if let Some(inner) = val
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .filter(|inner| !closing_escaped(inner))
        {
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some(n @ ('"' | '\\')) => out.push(n),
                        Some(n) => {
                            out.push('\\');
                            out.push(n);
                        }
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            return out;
        }
        if let Some(inner) = val.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            return inner.replace("''", "'");
        }
    }
    val.to_string()
}

/// Get file metadata: (mtime, ctime/birthtime, size)
fn file_meta(path: &Path) -> (SystemTime, SystemTime, u64) {
    let meta = fs::metadata(path).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let ctime = meta
        .as_ref()
        .and_then(|m| m.created().ok())
        .unwrap_or(mtime);
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    (mtime, ctime, size)
}

/// Agent label for files that many agents read (project-level `AGENTS.md`,
/// `.agents/skills/`).
const SHARED_AGENT: &str = "shared";
/// Project label for files that apply to every project.
const GLOBAL_PROJECT: &str = "(global)";

/// Shared files outside any project.
fn shared_global_sources(home: &Path) -> Vec<MemorySource> {
    vec![MemorySource::new(
        &home.join(".agents"),
        "skills/*/SKILL.md",
        MemoryKind::Skill,
    )]
}

/// Shared files in a project directory.
fn shared_project_sources(dir: &Path) -> Vec<MemorySource> {
    vec![
        MemorySource::new(dir, "AGENTS.md", MemoryKind::Instruction),
        MemorySource::new(dir, ".agents/skills/*/SKILL.md", MemoryKind::Skill),
    ]
}

/// Plugins of active built-in agents that know where their memory lives.
fn memory_plugins() -> Vec<&'static dyn AgentPlugin> {
    config::active_agents()
        .filter(|a| a.is_builtin && a.plugin.can_memory())
        .map(|a| a.plugin)
        .collect()
}

/// Read one memory/instruction file into an entry. Empty files are skipped.
fn read_entry(
    path: PathBuf,
    agent: &'static str,
    project: &str,
    kind: MemoryKind,
) -> Option<MemoryEntry> {
    let content = fs::read_to_string(&path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    let (mtime, ctime, size) = file_meta(&path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let non_empty = |s: String, fallback: String| if s.is_empty() { fallback } else { s };

    let (memory_type, name, description, body) = match kind {
        MemoryKind::Instruction => ("instruction".to_string(), file_name, String::new(), content),
        MemoryKind::Rule => {
            let (fm, body) = parse_frontmatter(&content);
            ("rule".to_string(), file_name, fm.description, body)
        }
        MemoryKind::Memory => {
            let (fm, body) = parse_frontmatter(&content);
            (
                non_empty(fm.memory_type, "memory".to_string()),
                non_empty(fm.name, stem),
                fm.description,
                body,
            )
        }
        MemoryKind::Skill => {
            let (fm, body) = parse_frontmatter(&content);
            let dir_name = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(stem);
            (
                "skill".to_string(),
                non_empty(fm.name, dir_name),
                fm.description,
                body,
            )
        }
    };

    Some(MemoryEntry {
        path,
        mtime,
        ctime,
        size,
        agent,
        project: project.to_string(),
        memory_type,
        name,
        description,
        body,
    })
}

/// Expand memory sources into entries. Skills are listed only on request
/// (`-t skill`), since installed skills can far outnumber memory files.
fn expand_sources(
    sources: Vec<MemorySource>,
    agent: &'static str,
    project: &str,
    with_skills: bool,
) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();
    for source in sources {
        if source.kind == MemoryKind::Skill && !with_skills {
            continue;
        }
        let mut paths: Vec<PathBuf> = glob::glob(&source.pattern())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|p| p.is_file())
            .collect();
        paths.sort();
        entries.extend(
            paths
                .into_iter()
                .filter_map(|p| read_entry(p, agent, project, source.kind)),
        );
    }
    entries
}

/// Project-level memory and instruction files in one directory.
fn collect_project_files(
    dir: &Path,
    plugins: &[&'static dyn AgentPlugin],
    with_skills: bool,
) -> Vec<MemoryEntry> {
    let project = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut entries = expand_sources(
        shared_project_sources(dir),
        SHARED_AGENT,
        &project,
        with_skills,
    );
    for plugin in plugins {
        entries.extend(expand_sources(
            plugin.project_memory_sources(dir),
            plugin.id(),
            &project,
            with_skills,
        ));
    }
    entries
}

/// Directories whose project files apply to `cwd`: the directory itself and
/// its git root, where agents also look for instruction files.
fn current_project_dirs(cwd: &str) -> Vec<PathBuf> {
    let dir = PathBuf::from(cwd);
    let mut dirs = vec![dir.clone()];
    if let Some(root) = dir.ancestors().find(|d| d.join(".git").exists()) {
        if root != dir {
            dirs.push(root.to_path_buf());
        }
    }
    dirs
}

/// Collect known project cwds from session files (for -a mode).
/// `--no-archived` hides sessions, not the projects they reveal.
fn collect_known_project_cwds() -> Vec<String> {
    let home = canonical_home();
    let files = collector::collect_all_files(0);
    let resolve_fields = vec![Field::Cwd];

    let cwds: HashSet<String> = files
        .par_iter()
        .filter_map(|(path, mtime)| {
            let plugin = agents::find_plugin_for_path(path);
            let fields = resolver::resolve_fields(
                path,
                plugin,
                *mtime,
                &home,
                &resolve_fields,
                &Default::default(),
            );
            fields.get(&Field::Cwd).filter(|v| !v.is_empty()).cloned()
        })
        .collect();

    cwds.into_iter().collect()
}

/// Build memory records for output.
pub fn build_memory_records(
    args: &MemoryResolvedArgs,
    filter: &FilterArgs,
) -> Result<Vec<BTreeMap<MemoryField, String>>, String> {
    let cwd = if let Some(ref d) = filter.dir {
        FilterArgs::resolve_dir(d)
    } else {
        std::env::current_dir()
            .ok()
            .and_then(|p| fs::canonicalize(&p).ok())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    let home = canonical_home();
    let plugins = memory_plugins();
    let with_skills = args.memory_type.as_deref() == Some("skill");

    // Collect all entries
    let mut entries: Vec<MemoryEntry> = Vec::new();

    // 1. Memory the agents keep per project (e.g. Claude auto memory). Agents
    //    key it by the directory they started in or by its repository root.
    let current_dirs = current_project_dirs(&cwd);
    let cwd_filters: Vec<Option<String>> = if filter.all {
        vec![None]
    } else {
        current_dirs
            .iter()
            .map(|d| Some(d.to_string_lossy().to_string()))
            .collect()
    };
    for plugin in &plugins {
        for cwd_filter in &cwd_filters {
            for file in plugin.agent_memory_files(&home, cwd_filter.as_deref()) {
                entries.extend(read_entry(
                    file.path,
                    plugin.id(),
                    &file.project,
                    MemoryKind::Memory,
                ));
            }
        }
    }

    // 2. Global files, collected first so that project scans can skip them
    let mut global_entries = Vec::new();
    for plugin in &plugins {
        global_entries.extend(expand_sources(
            plugin.global_memory_sources(&home),
            plugin.id(),
            GLOBAL_PROJECT,
            with_skills,
        ));
    }
    global_entries.extend(expand_sources(
        shared_global_sources(&home),
        SHARED_AGENT,
        GLOBAL_PROJECT,
        with_skills,
    ));
    // A project scan of the home directory itself reaches global files
    // (e.g. `~/.claude/CLAUDE.md` as `<dir>/.claude/CLAUDE.md`); those stay
    // global. Only the literal path counts, so a project file symlinked
    // to/from a global one keeps its project attribution.
    let global_paths: HashSet<(PathBuf, &'static str)> = global_entries
        .iter()
        .map(|e| (e.path.clone(), e.agent))
        .collect();

    // 3. Project files (listed before global ones, see dedup below)
    let project_dirs: Vec<PathBuf> = if filter.all {
        // Scan known project cwds (sorted so duplicate resolution is stable)
        let mut cwds = collect_known_project_cwds();
        cwds.sort_unstable();
        let mut seen = HashSet::new();
        cwds.iter()
            .filter_map(|dir| fs::canonicalize(dir).ok()) // skip non-existent dirs
            // Only collect from dirs under home
            .filter(|c| c.starts_with(&home) && c.is_dir())
            .filter(|c| seen.insert(c.clone()))
            .collect()
    } else {
        current_dirs
    };
    let project_entries: Vec<Vec<MemoryEntry>> = project_dirs
        .par_iter()
        .map(|dir| collect_project_files(dir, &plugins, with_skills))
        .collect();
    entries.extend(
        project_entries
            .into_iter()
            .flatten()
            .filter(|e| !global_paths.contains(&(e.path.clone(), e.agent))),
    );
    entries.extend(global_entries);

    if entries.is_empty() {
        return Err("No memory files found.".to_string());
    }

    let since = filter.since_time()?;
    let until = filter.until_time()?;

    let query_re = filter
        .query
        .as_ref()
        .map(|q| {
            Regex::new(&format!("(?i){}", q)).map_err(|e| format!("Invalid regex '{}': {}", q, e))
        })
        .transpose()?;

    let home = canonical_home();
    let home_str = home.to_string_lossy().to_string();

    // The same file can be reached twice for one agent (e.g. through a
    // symlinked project path). After filtering, keep the first occurrence
    // (project before global). One file shared by several agents (e.g.
    // ~/.codex/AGENTS.md -> ~/AGENTS.md) stays listed once per agent.
    let mut seen_paths = HashSet::new();

    let mut records: Vec<BTreeMap<MemoryField, String>> = entries
        .into_iter()
        .filter_map(|entry| {
            // Agent filter
            // Shared files (project AGENTS.md) are read by several agents,
            // so they match any agent filter.
            if let Some(ref agent_filter) = filter.agent {
                if entry.agent != *agent_filter && entry.agent != SHARED_AGENT {
                    return None;
                }
            }

            // Time range filter
            if let Some(ref since) = since {
                if &entry.mtime < since {
                    return None;
                }
            }
            if let Some(ref until) = until {
                if &entry.mtime > until {
                    return None;
                }
            }

            // Project filter
            if let Some(ref project_filter) = filter.project {
                if entry.project != *project_filter {
                    return None;
                }
            }

            // --type filter
            if let Some(ref t) = args.memory_type {
                if entry.memory_type != *t {
                    return None;
                }
            }

            // Query filter
            let matched_snippet = if let Some(ref re) = query_re {
                let mut snippet = String::new();
                for line in entry.body.lines() {
                    if re.is_match(line) {
                        if !snippet.is_empty() {
                            snippet.push_str(" | ");
                        }
                        snippet.push_str(line.trim());
                        if snippet.len() > 200 {
                            break;
                        }
                    }
                }
                if snippet.is_empty()
                    && !re.is_match(&entry.name)
                    && !re.is_match(&entry.description)
                {
                    return None;
                }
                snippet
            } else {
                String::new()
            };

            let key = fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone());
            if !seen_paths.insert((key, entry.agent)) {
                return None;
            }

            let resolve = |field: &MemoryField| -> String {
                match field {
                    MemoryField::Agent => entry.agent.to_string(),
                    MemoryField::Project => entry.project.clone(),
                    MemoryField::Type => entry.memory_type.clone(),
                    MemoryField::Name => entry.name.clone(),
                    MemoryField::Description => entry.description.clone(),
                    MemoryField::ModifiedAt => format_mtime(entry.mtime),
                    MemoryField::CreatedAt => format_mtime(entry.ctime),
                    MemoryField::Size => entry.size.to_string(),
                    MemoryField::Lines => entry.body.lines().count().to_string(),
                    MemoryField::FileName => entry
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    MemoryField::Path => {
                        let p = entry.path.to_string_lossy();
                        p.strip_prefix(home_str.as_str())
                            .map(|s| format!("~{s}"))
                            .unwrap_or_else(|| p.to_string())
                    }
                    MemoryField::Body => entry.body.clone(),
                    MemoryField::Matched => matched_snippet.clone(),
                }
            };

            let mut record = BTreeMap::new();
            for field in &args.fields {
                record.insert(*field, resolve(field));
            }

            // Ensure sort field is present
            record
                .entry(args.sort_field)
                .or_insert_with(|| resolve(&args.sort_field));

            Some(record)
        })
        .collect();

    if records.is_empty() {
        return Err("No memory files found.".to_string());
    }

    let numeric = args.sort_field.is_numeric();
    match args.sort_order {
        SortOrder::Desc => records.sort_by(|a, b| {
            output::compare_field_values(b.get(&args.sort_field), a.get(&args.sort_field), numeric)
        }),
        SortOrder::Asc => records.sort_by(|a, b| {
            output::compare_field_values(a.get(&args.sort_field), b.get(&args.sort_field), numeric)
        }),
    }

    Ok(records)
}

/// Entry point for `ah memory`.
pub fn run(args: MemoryResolvedArgs, filter: &FilterArgs) -> Result<(), String> {
    // Validate filter inputs early so invalid args fail fast even when local is empty
    filter.since_time()?;
    filter.until_time()?;
    if let Some(ref q) = filter.query {
        Regex::new(&format!("(?i){}", q)).map_err(|e| format!("Invalid regex '{}': {}", q, e))?;
    }

    let query = filter.query.clone().unwrap_or_default();
    let mut records = match build_memory_records(&args, filter) {
        Ok(r) => r,
        Err(e) if !filter.remote.is_empty() && crate::is_empty_result_error(&e) => {
            if crate::color::is_debug() {
                eprintln!("[debug] local memory: {}", e);
            }
            Vec::new()
        }
        Err(e) => return Err(e),
    };

    // Merge remote memory records if --remote is specified
    if !filter.remote.is_empty() {
        let remotes = crate::remote::resolve_remotes(&filter.remote)?;
        let mut remote_fields = args.fields.clone();
        if !remote_fields.contains(&args.sort_field) {
            remote_fields.push(args.sort_field);
        }
        let remote_records = crate::remote::fetch_remote_memory(
            &remotes,
            &remote_fields,
            filter,
            args.memory_type.as_deref(),
        );
        records.extend(remote_records);

        // Re-sort after merging
        let sf = args.sort_field;
        let numeric = sf.is_numeric();
        match args.sort_order {
            crate::cli::SortOrder::Desc => records
                .sort_by(|a, b| output::compare_field_values(b.get(&sf), a.get(&sf), numeric)),
            crate::cli::SortOrder::Asc => records
                .sort_by(|a, b| output::compare_field_values(a.get(&sf), b.get(&sf), numeric)),
        }
    }

    if records.is_empty() {
        return Err("No memory files found.".to_string());
    }

    output::output_memory(&records, &args.fields, &args.output_format, &query);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::claude::encode_path_for_claude;
    use crate::agents::common::decode_claude_project;

    #[test]
    fn test_parse_frontmatter_normal() {
        let content = "---\nname: test memory\ndescription: a test\ntype: feedback\n---\n\nBody content here.";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.name, "test memory");
        assert_eq!(fm.description, "a test");
        assert_eq!(fm.memory_type, "feedback");
        assert_eq!(body, "Body content here.");
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "Just plain text.";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.name, "");
        assert_eq!(fm.memory_type, "");
        assert_eq!(body, "Just plain text.");
    }

    #[test]
    fn test_parse_frontmatter_empty() {
        let content = "";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.name, "");
        assert_eq!(body, "");
    }

    #[test]
    fn test_parse_frontmatter_unclosed() {
        let content = "---\nname: test\nSome body without closing";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.name, "");
        assert_eq!(body, content);
    }

    #[test]
    fn test_encode_path_for_claude() {
        assert_eq!(
            encode_path_for_claude("/Users/you/src/github.com/org/myapp"),
            "-Users-you-src-github-com-org-myapp"
        );
        assert_eq!(
            encode_path_for_claude("/data/home/u/src/org/pce_bc_api"),
            "-data-home-u-src-org-pce-bc-api"
        );
        assert_eq!(encode_path_for_claude("/tmp/a b+c"), "-tmp-a-b-c");
        // one dash per UTF-16 code unit (a surrogate pair becomes two)
        assert_eq!(encode_path_for_claude("/tmp/日本"), "-tmp---");
        assert_eq!(encode_path_for_claude("/tmp/😀"), "-tmp---");
    }

    #[test]
    fn test_unquote_scalar() {
        assert_eq!(unquote_scalar(r#""quoted desc""#), "quoted desc");
        assert_eq!(unquote_scalar("'it''s'"), "it's");
        assert_eq!(unquote_scalar(r#""a \"b\" \\ c""#), r#"a "b" \ c"#);
        assert_eq!(unquote_scalar("plain"), "plain");
        assert_eq!(unquote_scalar(r#""unbalanced"#), r#""unbalanced"#);
        assert_eq!(unquote_scalar(r#""foo\""#), r#""foo\""#);
        assert_eq!(unquote_scalar(r#""foo\\""#), r#"foo\"#);
        assert_eq!(unquote_scalar(r#"""#), r#"""#);
    }

    #[test]
    fn test_parse_frontmatter_quoted_values() {
        let content = "---\nname: \"q name\"\ndescription: 'single'\ntype: \"feedback\"\n---\nbody";
        let (fm, _) = parse_frontmatter(content);
        assert_eq!(fm.name, "q name");
        assert_eq!(fm.description, "single");
        assert_eq!(fm.memory_type, "feedback");
    }

    #[test]
    fn test_decode_project_name() {
        assert_eq!(
            decode_claude_project("-Users-you-src-github.com-org-myapp"),
            "myapp"
        );
    }

    #[test]
    fn test_decode_project_name_home_prefix() {
        assert_eq!(decode_claude_project("-home-user-projects-myapp"), "myapp");
    }
}

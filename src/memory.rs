//! Memory and instruction file listing and search (shared by `ah memory`).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use regex::Regex;

use crate::agents;
use crate::agents::common::{canonical_home, decode_claude_project, format_mtime};
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
                "name" => fm.name = val.to_string(),
                "description" => fm.description = val.to_string(),
                "type" => fm.memory_type = val.to_string(),
                _ => {}
            }
        }
    }

    (fm, body)
}

/// Encode a filesystem path to Claude's project directory naming convention.
/// `/Users/you/src/github.com/foo` → `-Users-you-src-github-com-foo`
fn encode_path_for_claude(path: &str) -> String {
    path.replace(['/', '.'], "-")
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

/// Instruction file definitions: (agent, filename)
const INSTRUCTION_FILES: &[(&str, &str)] = &[
    ("claude", "CLAUDE.md"),
    ("codex", "AGENTS.md"),
    ("gemini", "GEMINI.md"),
    ("cursor", ".cursorrules"),
];

/// Collect Claude memory files from ~/.claude/projects/*/memory/*.md
fn collect_claude_memory_files(all: bool, cwd: &str) -> Vec<MemoryEntry> {
    let home = canonical_home();
    let claude_base = config::resolve_agent_base("claude").unwrap_or_else(|| home.join(".claude"));
    let projects_dir = claude_base.join("projects");

    let encoded_cwd = if !all {
        Some(encode_path_for_claude(cwd))
    } else {
        None
    };

    let pattern = format!("{}/*/memory/*.md", projects_dir.display());
    let mut results = Vec::new();

    for entry in glob::glob(&pattern).into_iter().flatten().flatten() {
        // Skip MEMORY.md (index file)
        if entry
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("MEMORY.md"))
        {
            continue;
        }

        let memory_dir = entry.parent();
        let project_dir = memory_dir.and_then(|p| p.parent());
        let encoded_name = project_dir
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if let Some(ref filter) = encoded_cwd {
            if encoded_name != *filter {
                continue;
            }
        }

        let (mtime, ctime, size) = file_meta(&entry);

        let content = match fs::read_to_string(&entry) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let (fm, body) = parse_frontmatter(&content);

        results.push(MemoryEntry {
            path: entry,
            mtime,
            ctime,
            size,
            agent: "claude",
            project: decode_claude_project(&encoded_name),
            memory_type: fm.memory_type,
            name: fm.name,
            description: fm.description,
            body,
        });
    }

    results
}

/// Collect global instruction files (e.g. ~/.claude/CLAUDE.md, ~/.codex/AGENTS.md)
fn collect_global_instructions() -> Vec<MemoryEntry> {
    let mut results = Vec::new();

    for &(agent, filename) in INSTRUCTION_FILES {
        let base = match config::resolve_agent_base(agent) {
            Some(b) => b,
            None => continue,
        };
        let path = base.join(filename);
        if !path.exists() {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) if !c.trim().is_empty() => c,
            _ => continue,
        };
        let (mtime, ctime, size) = file_meta(&path);

        results.push(MemoryEntry {
            path,
            mtime,
            ctime,
            size,
            agent,
            project: "(global)".to_string(),
            memory_type: "instruction".to_string(),
            name: filename.to_string(),
            description: String::new(),
            body: content,
        });
    }

    results
}

/// Collect project-level instruction files from a directory.
fn collect_project_instructions(dir: &Path) -> Vec<MemoryEntry> {
    let mut results = Vec::new();
    let project = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    for &(agent, filename) in INSTRUCTION_FILES {
        let path = dir.join(filename);
        if !path.exists() {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) if !c.trim().is_empty() => c,
            _ => continue,
        };
        let (mtime, ctime, size) = file_meta(&path);

        results.push(MemoryEntry {
            path,
            mtime,
            ctime,
            size,
            agent,
            project: project.clone(),
            memory_type: "instruction".to_string(),
            name: filename.to_string(),
            description: String::new(),
            body: content,
        });
    }

    results
}

/// Collect known project cwds from session files (for -a mode).
fn collect_known_project_cwds() -> Vec<String> {
    let home = canonical_home();
    let files = collector::collect_files(0);
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

    // Collect all entries
    let mut entries: Vec<MemoryEntry> = Vec::new();

    // 1. Claude memory files
    entries.extend(collect_claude_memory_files(filter.all, &cwd));

    // 2. Global instruction files
    entries.extend(collect_global_instructions());

    // 3. Project instruction files
    if filter.all {
        // Scan known project cwds
        let cwds = collect_known_project_cwds();
        let home = canonical_home();
        let mut seen = HashSet::new();
        for dir in &cwds {
            let p = Path::new(dir);
            let canonical = match fs::canonicalize(p) {
                Ok(c) => c,
                Err(_) => continue, // skip non-existent dirs
            };
            // Only collect from dirs under home
            if !canonical.starts_with(&home) {
                continue;
            }
            if seen.insert(dir.clone()) && p.is_dir() {
                entries.extend(collect_project_instructions(&canonical));
            }
        }
    } else {
        // Current directory only
        entries.extend(collect_project_instructions(Path::new(&cwd)));
    }

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

    let mut records: Vec<BTreeMap<MemoryField, String>> = entries
        .into_iter()
        .filter_map(|entry| {
            // Agent filter
            if let Some(ref agent_filter) = filter.agent {
                if entry.agent != *agent_filter {
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

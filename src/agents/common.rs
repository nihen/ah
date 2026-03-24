use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use memmap2::Mmap;
use regex::Regex;

/// Resolve and canonicalize the user's home directory.
pub fn canonical_home() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    fs::canonicalize(&home).unwrap_or(home)
}

pub fn format_mtime(mtime: SystemTime) -> String {
    let dt: DateTime<Local> = mtime.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

pub fn strip_home(path: &str, home: &Path) -> String {
    let home_str = home.to_string_lossy();
    if path.starts_with(home_str.as_ref()) {
        path[home_str.len()..].trim_start_matches('/').to_string()
    } else {
        path.to_string()
    }
}

/// Regex to strip home-directory prefix from Claude project directory names.
/// Matches patterns like `-Users-you-` or `-home-user-`.
pub static RE_HOME_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-(Users|data-home|home)-[^-]+-").unwrap());

/// Decode a Claude-encoded project directory name to a project basename.
/// e.g. `-Users-you-src-github-com-org-myapp` → `myapp`
pub fn decode_claude_project(encoded_dir: &str) -> String {
    let full = RE_HOME_PREFIX.replace(encoded_dir, "").replace('-', "/");
    full.rsplit('/').next().unwrap_or(&full).to_string()
}

pub fn mmap_file(path: &Path) -> Option<Mmap> {
    let file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if meta.len() == 0 {
        return None;
    }
    unsafe { Mmap::map(&file) }.ok()
}

pub fn for_each_jsonl_value(path: &Path, visit: impl FnMut(&serde_json::Value) -> bool) {
    let mmap = match mmap_file(path) {
        Some(mmap) => mmap,
        None => return,
    };
    for_each_jsonl_value_bytes(&mmap, visit);
}

pub fn for_each_jsonl_value_bytes(data: &[u8], mut visit: impl FnMut(&serde_json::Value) -> bool) {
    for line_bytes in data.split(|&b| b == b'\n') {
        if line_bytes.len() < 2 {
            continue;
        }
        if let Ok(line) = std::str::from_utf8(line_bytes) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if !visit(&val) {
                    return;
                }
            }
        }
    }
}

pub fn read_first_line_json(path: &Path) -> Option<serde_json::Value> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

pub fn first_text_part(val: &serde_json::Value) -> Option<&str> {
    val.as_str()
        .or_else(|| val.pointer("/0/text").and_then(|v| v.as_str()))
        .or_else(|| val.pointer("/content/0/text").and_then(|v| v.as_str()))
        .or_else(|| {
            // e.g. Gemini `[{"text":"..."}]`, or Cursor `[{type:text,...},{tool_use,...}]`
            val.as_array().and_then(|arr| {
                arr.iter().find_map(|item| {
                    if item.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        return None;
                    }
                    item.get("text").and_then(|v| v.as_str())
                })
            })
        })
}

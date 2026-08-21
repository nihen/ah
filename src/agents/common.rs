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

/// Strip the home directory prefix from `path` (path-boundary aware:
/// `/home/al` does not match `/home/alice/repo`).
pub fn strip_home(path: &str, home: &Path) -> String {
    let home_str = home.to_string_lossy();
    let home_str = home_str.trim_end_matches('/');
    if let Some(rest) = path.strip_prefix(home_str) {
        if rest.is_empty() || rest.starts_with('/') {
            return rest.trim_start_matches('/').to_string();
        }
    }
    path.to_string()
}

/// Canonicalize `path` if it exists on disk (resolves symlinks such as
/// `/home/user` → `/data/home/user` so that it matches the canonicalized cwd
/// filter); otherwise return it unchanged.
pub fn canonicalize_if_exists(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// A session/conversation id is safe to pass as a positional CLI argument
/// (must not be empty or look like an option).
pub fn is_safe_cli_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('-')
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

/// Extract the body wrapped in `<tag>…</tag>` from a raw user message.
/// Several agents wrap the real user text in a tag (Cursor: `user_query`,
/// Grok: `user_query`, Antigravity: `USER_REQUEST`) and append metadata
/// blocks after it. If the tag is absent and the text does not look like
/// injected markup (i.e. does not start with `<`), the trimmed text is
/// returned as-is.
pub fn tagged_user_body<'a>(raw: &'a str, tag: &str) -> Option<&'a str> {
    let t = raw.trim();
    // Locate `<tag>` / `</tag>` without allocating: scan '<' positions and
    // compare the following bytes against the tag in place.
    let open_at = t
        .match_indices('<')
        .find(|(i, _)| {
            let rest = &t[i + 1..];
            rest.starts_with(tag) && rest[tag.len()..].starts_with('>')
        })
        .map(|(i, _)| i);
    let close_at = t
        .rmatch_indices("</")
        .find(|(i, _)| {
            let rest = &t[i + 2..];
            rest.starts_with(tag) && rest[tag.len()..].starts_with('>')
        })
        .map(|(i, _)| i);
    if let (Some(i), Some(j)) = (open_at, close_at) {
        let start = i + 1 + tag.len() + 1;
        if j >= start {
            let inner = t[start..j].trim();
            if !inner.is_empty() {
                return Some(inner);
            }
        }
    }
    if !t.starts_with('<') {
        return Some(t);
    }
    None
}

/// Decode a percent-encoded string (e.g. `%2Fdata%2Fhome` → `/data/home`).
/// Invalid escapes are passed through unchanged.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_home_is_path_boundary_aware() {
        let home = Path::new("/home/al");
        assert_eq!(strip_home("/home/al/repo", home), "repo");
        assert_eq!(strip_home("/home/al", home), "");
        assert_eq!(strip_home("/home/alice/repo", home), "/home/alice/repo");
    }

    #[test]
    fn safe_cli_id_rejects_option_like_ids() {
        assert!(is_safe_cli_id("0192-aaaa"));
        assert!(!is_safe_cli_id("--always-approve"));
        assert!(!is_safe_cli_id(""));
    }

    #[test]
    fn tagged_user_body_extracts_or_passes_through() {
        assert_eq!(
            tagged_user_body(
                "<user_query>\nhi\n</user_query>\n<meta>x</meta>",
                "user_query"
            ),
            Some("hi")
        );
        assert_eq!(tagged_user_body("plain", "user_query"), Some("plain"));
        // a longer tag sharing the prefix must not match
        assert_eq!(
            tagged_user_body("<user_query_extra>x</user_query_extra>", "user_query"),
            None
        );
        assert_eq!(
            tagged_user_body(
                "<USER_REQUEST>\nこんにちは\n</USER_REQUEST>\n<ADDITIONAL_METADATA>t</ADDITIONAL_METADATA>",
                "USER_REQUEST"
            ),
            Some("こんにちは")
        );
        assert_eq!(
            tagged_user_body("<system>injected</system>", "user_query"),
            None
        );
    }

    #[test]
    fn percent_decode_handles_invalid_escapes() {
        assert_eq!(percent_decode("%2Fdata%2Fhome"), "/data/home");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }
}

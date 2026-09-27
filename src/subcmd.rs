use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::agents;
use crate::cli::{Field, FieldFilter, SearchMode, SortOrder};
use crate::collector;
use crate::pipeline;
use crate::resolver;

struct ResolveLookupOpts {
    since: Option<SystemTime>,
    until: Option<SystemTime>,
    require_resume_cmd: bool,
}

/// Resolve a session file path from the given options.
///
/// Priority:
/// 1. Session (ID or path, already read by `read_session_ref`)
/// 2. Query / filters → latest matching session (via pipeline)
///
/// Stdin is not read here: callers resolve it once with `read_session_ref`,
/// so an empty first line cannot make a second read pick up the next one.
pub fn resolve_session(
    session: Option<&str>,
    query: Option<&str>,
    filters: &[FieldFilter],
    home: &Path,
    search_mode: SearchMode,
    since: Option<SystemTime>,
    until: Option<SystemTime>,
) -> Result<PathBuf, String> {
    resolve_session_inner(
        session,
        query,
        filters,
        home,
        search_mode,
        ResolveLookupOpts {
            since,
            until,
            require_resume_cmd: false,
        },
    )
}

pub fn resolve_resumable_session(
    session: Option<&str>,
    query: Option<&str>,
    filters: &[FieldFilter],
    home: &Path,
    search_mode: SearchMode,
    since: Option<SystemTime>,
    until: Option<SystemTime>,
) -> Result<PathBuf, String> {
    resolve_session_inner(
        session,
        query,
        filters,
        home,
        search_mode,
        ResolveLookupOpts {
            since,
            until,
            require_resume_cmd: true,
        },
    )
}

fn resolve_session_inner(
    session: Option<&str>,
    query: Option<&str>,
    filters: &[FieldFilter],
    home: &Path,
    search_mode: SearchMode,
    opts: ResolveLookupOpts,
) -> Result<PathBuf, String> {
    if let Some(session_ref) = session
        .map(normalize_session_ref)
        .filter(|session_ref| !session_ref.is_empty())
    {
        return resolve_session_ref(&session_ref, home);
    }

    // 2. Query / filters → latest via pipeline
    let q = query.unwrap_or("");
    let not_found_msg = if q.is_empty() {
        "No session found matching filters".to_string()
    } else {
        format!("No session found matching: {}", q)
    };

    let result = pipeline::run_pipeline(&pipeline::PipelineParams {
        resolve_fields: resolve_fields_for_lookup(opts.require_resume_cmd),
        resolve_opts: resolver::ResolveOpts::default(),
        filters: filters.to_vec(),
        since: opts.since,
        until: opts.until,
        query: q.to_string(),
        search_mode,
        sort_field: Field::ModifiedAt,
        sort_order: SortOrder::Desc,
        collect_limit: 0, // scan all: filter/search runs after collect
        running: false,
        require_resume_cmd: opts.require_resume_cmd,
    })?;

    match result.sessions.into_iter().next() {
        Some(s) => Ok(s.path),
        None => Err(not_found_msg),
    }
}

/// Read an explicit session reference from the positional argument or stdin.
///
/// A positional session always wins and stdin is left untouched, so
/// `while read id; do ah show "$id"; done < ids` and callers whose stdin
/// never closes behave as expected. An empty positional session (e.g. an
/// unset `"$id"`) is an error rather than a silent fallback to the latest
/// session. `-` reads the reference from stdin explicitly and fails when
/// stdin has no reference. Without a positional session, a non-terminal
/// stdin is read for one line; an empty line or EOF falls back to the
/// query/filter lookup.
///
/// The returned value is intentionally left raw — TSV escape decoding is
/// done lazily by `resolve_session_ref` (literal-first, then unescaped
/// fallback) and by remote dispatch sites, so raw paths piped from
/// non-`ah` producers (e.g. `echo C:\\temp\\sess.jsonl | ah show`) still
/// resolve correctly.
pub fn read_session_ref(session: Option<&str>) -> Result<Option<String>, String> {
    match session {
        Some("-") => read_stdin_session_ref()?
            .map(Some)
            .ok_or_else(|| "No session reference on stdin (SESSION is '-')".to_string()),
        Some(session) => {
            let session_ref = normalize_session_ref(session);
            if session_ref.is_empty() {
                return Err("SESSION is empty".into());
            }
            Ok(Some(session_ref))
        }
        None if !std::io::IsTerminal::is_terminal(&std::io::stdin()) => read_stdin_session_ref(),
        None => Ok(None),
    }
}

fn read_stdin_session_ref() -> Result<Option<String>, String> {
    let field = read_stdin_first_field().map_err(|e| format!("Failed to read stdin: {e}"))?;
    stdin_field_to_session_ref(field)
}

fn stdin_field_to_session_ref(field: StdinField) -> Result<Option<String>, String> {
    let text = String::from_utf8(field.bytes)
        .map_err(|_| "Session reference on stdin is not valid UTF-8".to_string())?;
    // Match `line.trim().split('\t').next()`: whitespace before a tab stays
    // in the field when a non-blank column follows (so `path: \ttail` is
    // still an empty LTSV value); otherwise `trim` also strips the tab and
    // the field loses its trailing whitespace.
    let text = if field.ended_by_tab {
        text.trim_start()
    } else {
        text.trim()
    };
    Ok(Some(normalize_session_ref(text)).filter(|s| !s.is_empty()))
}

/// Longest session reference (first TSV field) accepted on stdin.
const MAX_SESSION_REF_LEN: usize = 64 * 1024;

#[derive(Debug)]
struct StdinField {
    bytes: Vec<u8>,
    /// The field ended at a tab followed by a non-blank column.
    ended_by_tab: bool,
}

/// Collects the first TSV field of one stdin line. Leading ASCII whitespace
/// (including tabs) is skipped, everything after the first tab is only
/// drained, and the field is capped at `MAX_SESSION_REF_LEN` so that long
/// trailing columns (e.g. `-o path,transcript`) do not matter.
#[derive(Default)]
struct FirstField {
    buf: Vec<u8>,
    done: bool,
    ended_by_tab: bool,
    tail_nonblank: bool,
    too_long: bool,
}

impl FirstField {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.done {
                if self.ended_by_tab && !self.tail_nonblank && !b.is_ascii_whitespace() {
                    self.tail_nonblank = true;
                }
                if self.tail_nonblank || !self.ended_by_tab {
                    return;
                }
                continue;
            }
            if self.buf.is_empty() && b.is_ascii_whitespace() {
                continue;
            }
            if b == b'\t' {
                self.done = true;
                self.ended_by_tab = true;
            } else if self.buf.len() < MAX_SESSION_REF_LEN {
                self.buf.push(b);
            } else {
                self.too_long = true;
                self.done = true;
            }
        }
    }

    fn finish(self) -> std::io::Result<StdinField> {
        if self.too_long {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session reference longer than {MAX_SESSION_REF_LEN} bytes"),
            ));
        }
        Ok(StdinField {
            bytes: self.buf,
            ended_by_tab: self.ended_by_tab && self.tail_nonblank,
        })
    }
}

/// Read the first field of one stdin line without consuming anything after
/// its newline, so the rest of a shared stdin (e.g.
/// `{ ah show; ah show; } < refs`) stays available to the next reader.
#[cfg(unix)]
fn read_stdin_first_field() -> std::io::Result<StdinField> {
    use std::io::Read;
    use std::os::unix::io::FromRawFd;

    // Bypass std's buffered stdin, which reads ahead past the newline.
    // ManuallyDrop keeps fd 0 open when the File goes out of scope.
    let mut stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    let mut field = FirstField::default();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => field.push(&byte),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    field.finish()
}

/// Non-Unix fallback: std's buffered stdin may read past the first line, so
/// a stdin shared by several `ah` invocations is not supported here.
#[cfg(not(unix))]
fn read_stdin_first_field() -> std::io::Result<StdinField> {
    read_first_field(&mut std::io::stdin().lock())
}

/// Read the first field of one line from a buffered reader, consuming the
/// line through its newline.
#[cfg_attr(unix, allow(dead_code))]
fn read_first_field(reader: &mut impl std::io::BufRead) -> std::io::Result<StdinField> {
    let mut field = FirstField::default();
    loop {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            break;
        }
        let newline = buf.iter().position(|&b| b == b'\n');
        field.push(&buf[..newline.unwrap_or(buf.len())]);
        let used = newline.map_or(buf.len(), |i| i + 1);
        reader.consume(used);
        if newline.is_some() {
            break;
        }
    }
    field.finish()
}

/// Resolve a session reference: try as file path first, then as session ID.
/// Raw-first ordering: literal paths (Windows `C:\foo`, Unix paths with
/// embedded `\t` chars piped from non-`ah` producers) win over the decoded
/// form so they resolve correctly. TSV-decoded fallback only kicks in when
/// the raw value isn't a real file or session ID — that's the
/// `ah ... -o path | ah show` round-trip path.
pub fn resolve_session_ref(s: &str, home: &Path) -> Result<PathBuf, String> {
    let s = strip_ltsv_prefix(s);

    // Try as file path
    let pb = PathBuf::from(s);
    if session_exists(&pb) {
        return Ok(pb);
    }

    // Strip surrounding quotes (e.g. from fzf preview passing shell-quoted paths)
    let unquoted = crate::output::strip_quotes(s);
    if unquoted != s {
        let pb = PathBuf::from(unquoted);
        if session_exists(&pb) {
            return Ok(pb);
        }
    }

    // TSV-decoded fallback (`\\` / `\t` / `\n` / `\r` → literal). Only
    // attempted after the raw value has failed both the file-path check
    // and the surrounding-quote strip, so raw paths from non-`ah` producers
    // round-trip without corruption.
    let unescaped = crate::output::unescape_tsv(unquoted);
    if unescaped != unquoted {
        let pb = PathBuf::from(&unescaped);
        if session_exists(&pb) {
            return Ok(pb);
        }
        if let Ok(p) = resolve_by_id(&unescaped, home) {
            return Ok(p);
        }
    }

    // Try as session ID (use unquoted value)
    resolve_by_id(unquoted, home)
}

/// A session reference exists as a file, or as a virtual session of a plugin
/// (e.g. `<opencode.db>/<session-id>`).
fn session_exists(path: &Path) -> bool {
    path.exists()
        || agents::find_plugin_for_path(path)
            .session_mtime(path)
            .is_some()
}

fn resolve_by_id(id: &str, home: &Path) -> Result<PathBuf, String> {
    // Several files can hold one session (e.g. a Gemini log migrated from
    // .json to .jsonl); pick the copy `ah log` lists. Explicit references
    // also reach archived sessions.
    let files = collector::collect_all_files(0);
    let resolve_fields = [Field::Id];
    let opts = resolver::ResolveOpts::default();

    // Preferred file per session: exact id matches, then id-prefix matches,
    // each keyed by (id, parent id). One id under several parents (e.g.
    // subagent transcripts of different sessions) is ambiguous.
    let mut exact: HashMap<String, (PathBuf, SystemTime)> = HashMap::new();
    let mut prefix_matches: HashMap<(String, String), (PathBuf, SystemTime)> = HashMap::new();
    let better = |a: &PathBuf, a_mtime: SystemTime, b: &PathBuf, b_mtime: SystemTime| {
        pipeline::copy_preference(a, a_mtime) > pipeline::copy_preference(b, b_mtime)
    };

    for (fpath, mtime) in &files {
        let plugin = agents::find_plugin_for_path(fpath);
        let fields = resolver::resolve_fields(fpath, plugin, *mtime, home, &resolve_fields, &opts);
        let Some(v) = fields.get(&Field::Id) else {
            continue;
        };
        if !v.starts_with(id) {
            continue;
        }
        let parent = plugin.parent_session_id(fpath).unwrap_or_default();
        // Files come newest first (ties by path, like `copy_preference`), so
        // the first dedicated file of a top-level session is the preferred
        // copy. Only subagent ids, which may repeat under other parents, and
        // secondary records keep looking.
        if v == id && parent.is_empty() && !plugin.is_secondary_record(fpath) {
            return Ok(fpath.clone());
        }
        let candidates = if v == id {
            exact.entry(parent)
        } else if v.starts_with(id) {
            match prefix_matches.entry((v.clone(), parent)) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if better(fpath, *mtime, &e.get().0, e.get().1) {
                        e.insert((fpath.clone(), *mtime));
                    }
                    continue;
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((fpath.clone(), *mtime));
                    continue;
                }
            }
        } else {
            continue;
        };
        match candidates {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if better(fpath, *mtime, &e.get().0, e.get().1) {
                    e.insert((fpath.clone(), *mtime));
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((fpath.clone(), *mtime));
            }
        }
    }

    let mut exact = exact.into_values();
    match (exact.next(), exact.next()) {
        (Some((path, _)), None) => return Ok(path),
        (Some(_), Some(_)) => {
            return Err(format!(
                "Ambiguous session id: {} (several sessions; pass the path instead)",
                id
            ));
        }
        (None, _) => {}
    }
    let mut matches = prefix_matches.into_values();
    match (matches.next(), matches.next()) {
        (None, _) => Err(format!("No session found for id: {}", id)),
        (Some((path, _)), None) => Ok(path),
        _ => Err(format!("Ambiguous session id prefix: {}", id)),
    }
}

fn normalize_session_ref(s: &str) -> String {
    let s = strip_ltsv_prefix(s);
    crate::output::strip_ansi(s).trim().to_string()
}

fn strip_ltsv_prefix(s: &str) -> &str {
    strip_ltsv_prefix_with(s, |candidate| {
        crate::remote::parse_remote_path(candidate).is_some()
    })
}

fn strip_ltsv_prefix_with<F>(s: &str, is_remote_ref: F) -> &str
where
    F: Fn(&str) -> bool,
{
    if is_remote_ref(s) {
        return s;
    }

    let Some(i) = s.find(':') else {
        return s;
    };
    let prefix = &s[..i];
    let after = &s[i + 1..];

    // Always strip the `path:` LTSV key (the only key `ah log -o path` emits)
    // when there's a non-empty value after it. This way:
    //   path:/abs/foo.jsonl     → /abs/foo.jsonl   (local path)
    //   path:mydev:/foo         → mydev:/foo       (remote ref preserved)
    //   path:typo:/foo          → typo:/foo        (lets check_unknown_remote
    //                                               surface "Unknown remote 'typo'"
    //                                               instead of "Unknown remote 'path'")
    if prefix == "path" && !after.is_empty() {
        after
    } else {
        s
    }
}

fn resolve_fields_for_lookup(require_resume_cmd: bool) -> Vec<Field> {
    let mut fields = vec![Field::Path, Field::ModifiedAt];
    if require_resume_cmd {
        fields.push(Field::ResumeCmd);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_field_reader_keeps_the_rest_and_bounds_only_the_first_field() {
        // A tiny buffer forces lines to span several fill_buf() calls.
        let exact = "a".repeat(MAX_SESSION_REF_LEN);
        let over = "b".repeat(MAX_SESSION_REF_LEN + 1);
        let long_tail = "t".repeat(MAX_SESSION_REF_LEN * 2);
        let input = format!("{exact}\n{over}\n \tref\t{long_tail}\nlast");
        let mut reader = std::io::BufReader::with_capacity(7, input.as_bytes());

        let mut next = || read_first_field(&mut reader);
        let field = next().unwrap();
        assert_eq!(field.bytes, exact.as_bytes());
        assert!(!field.ended_by_tab);
        assert_eq!(next().unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        // Leading whitespace is skipped; a long trailing column is drained.
        let field = next().unwrap();
        assert_eq!(field.bytes, b"ref");
        assert!(field.ended_by_tab);
        assert_eq!(next().unwrap().bytes, b"last");
        assert_eq!(next().unwrap().bytes, b"");
    }

    #[test]
    fn stdin_field_trimming_matches_line_trim_then_split() {
        // normalize_session_ref consults configured remotes.
        crate::config::init(&crate::agents::common::canonical_home());
        // Old behavior: normalize(line.trim().split('\t').next()).
        let old = |line: &str| {
            let first = line.trim().split('\t').next().unwrap_or("");
            Some(normalize_session_ref(first)).filter(|s| !s.is_empty())
        };
        let new = |line: &str| {
            let mut reader = std::io::BufReader::new(line.as_bytes());
            stdin_field_to_session_ref(read_first_field(&mut reader).unwrap()).unwrap()
        };
        for line in [
            "/a/b.jsonl",
            "  /a/b.jsonl  \r\n",
            " \t/a/b.jsonl\tx",
            "/a/b.jsonl \tx",
            "path:/a/b.jsonl\tx",
            "path: \ttail",
            "path: \t",
            "path: \t\n",
            "path: \t \r\n",
            "path: \t \t x",
            "path: ",
            "path:",
            "/a/b.jsonl\t\t",
            "",
            "\t\t",
        ] {
            assert_eq!(new(line), old(line), "{line:?}");
        }
    }

    #[test]
    fn resumable_lookup_resolves_resume_cmd_field() {
        assert_eq!(
            resolve_fields_for_lookup(true),
            vec![Field::Path, Field::ModifiedAt, Field::ResumeCmd]
        );
    }

    #[test]
    fn non_resumable_lookup_keeps_default_fields() {
        assert_eq!(
            resolve_fields_for_lookup(false),
            vec![Field::Path, Field::ModifiedAt]
        );
    }

    #[test]
    fn strip_ltsv_prefix_keeps_remote_refs() {
        assert_eq!(
            strip_ltsv_prefix_with("mydev:/tmp/session.jsonl", |s| s.starts_with("mydev:/")),
            "mydev:/tmp/session.jsonl"
        );
    }

    #[test]
    fn resolve_session_ref_prefers_literal_path_over_decoded() {
        // Raw paths from non-`ah` producers (e.g. `echo /tmp/foo\\tbar |
        // ah show` where the literal `\t` is part of the file name) must
        // resolve before the TSV-decoded fallback runs, so a real on-disk
        // `\t` filename is found rather than a phantom `<TAB>` path.
        let tmp = tempfile::tempdir().unwrap();
        let raw_path = tmp.path().join("raw\\tfile.jsonl");
        std::fs::write(&raw_path, "").unwrap();
        let resolved =
            resolve_session_ref(raw_path.to_str().unwrap(), tmp.path()).expect("literal path");
        assert_eq!(resolved, raw_path);
    }

    #[test]
    fn resolve_session_ref_decodes_tsv_when_literal_missing() {
        // The matching round-trip case: `escape_tsv` emitted `\\t` for a
        // file whose actual on-disk name has a TAB; piping that back must
        // hit the unescape fallback and resolve the TAB-named file.
        crate::config::init(&crate::agents::common::canonical_home());
        let tmp = tempfile::tempdir().unwrap();
        let real_path = tmp.path().join("real\ttab.jsonl"); // literal TAB
        std::fs::write(&real_path, "").unwrap();
        let escaped = format!("{}/real\\ttab.jsonl", tmp.path().display());
        let resolved = resolve_session_ref(&escaped, tmp.path()).expect("decoded fallback");
        assert_eq!(resolved, real_path);
    }

    #[test]
    fn strip_ltsv_prefix_preserves_remote_refs_inside_ltsv_values() {
        assert_eq!(
            strip_ltsv_prefix_with("path:mydev:/tmp/session.jsonl", |s| s
                .starts_with("mydev:/")),
            "mydev:/tmp/session.jsonl"
        );
    }

    #[test]
    fn strip_ltsv_prefix_keeps_unknown_remote_prefix() {
        // `badRemote:/foo` is not a known LTSV key (`path:`), so the prefix
        // must NOT be stripped. The caller (`check_unknown_remote`) will then
        // be able to surface "Unknown remote 'badRemote'".
        assert_eq!(
            strip_ltsv_prefix_with("badRemote:/foo/bar.jsonl", |_| false),
            "badRemote:/foo/bar.jsonl"
        );
    }

    #[test]
    fn strip_ltsv_prefix_strips_path_key_with_local_path() {
        assert_eq!(
            strip_ltsv_prefix_with("path:/abs/foo.jsonl", |_| false),
            "/abs/foo.jsonl"
        );
    }
}

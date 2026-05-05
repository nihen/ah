use std::io::{Seek, SeekFrom};

use crate::agents::common::canonical_home;
use crate::agents::{self, MessageRole};
use crate::cli::{FilterArgs, ShowArgs, ShowFormat};
use crate::color::{self, BOLD, DIM, RESET};
use crate::output::{strip_ansi, strip_quotes};
use crate::remote;
use crate::resolver;
use crate::subcmd;

/// Compile a case-insensitive matcher for the user pattern. The pattern is
/// `regex::escape`d before compilation, so it matches literal text — regex
/// metacharacters like `.` and `*` have no special meaning. Empty pattern
/// returns `Ok(None)`. Compilation failures (e.g. exceeding the regex size
/// limit) are surfaced as `Err` so `--highlight` doesn't silently no-op.
fn compile_highlight(pattern: &str) -> Result<Option<regex::Regex>, String> {
    if pattern.is_empty() {
        return Ok(None);
    }
    regex::RegexBuilder::new(&regex::escape(pattern))
        .case_insensitive(true)
        .build()
        .map(Some)
        .map_err(|e| format!("invalid --highlight pattern: {}", e))
}

/// Apply yellow-background highlighting to matching text.
/// Sets black fg + bright-yellow bg, and resets only fg/bg on exit
/// (\x1b[39;49m) so any surrounding DIM/BOLD attributes are preserved.
fn highlight_text(text: &str, re: &regex::Regex) -> String {
    const HL: &str = "\x1b[30;103m";
    const HL_END: &str = "\x1b[39;49m";
    re.replace_all(text, |caps: &regex::Captures| {
        format!("{}{}{}", HL, &caps[0], HL_END)
    })
    .into_owned()
}

pub fn run(args: ShowArgs, filter: &FilterArgs) -> Result<(), String> {
    let home = canonical_home();
    let explicit_session = subcmd::read_session_ref(args.session.as_deref());

    if let Some(session) = explicit_session.as_deref() {
        let unquoted = strip_quotes(session);
        if let Some((remote_def, remote_path)) = remote::parse_remote_path(unquoted) {
            remote::exec_remote_show(remote_def, remote_path, &args, filter);
        }
        remote::check_unknown_remote(unquoted)?;
    }

    // Only require an explicit REMOTE:REF when no session was given. With an
    // explicit local path (e.g. picked from interactive `--remote` mixed list),
    // we proceed against the local file even if `filter.remote` is set.
    if explicit_session.is_none() && !filter.remote.is_empty() {
        return Err(
            "Remote session lookup for 'show' requires an explicit REMOTE:REF session.\n\
             Example: ah show mydev:/home/user/.claude/sessions/<id>"
                .into(),
        );
    }

    let format = args.format();
    let follow = args.follow;
    let meta_fields = args.meta_fields()?;

    // Validate --follow early before any I/O or session resolution
    if follow && !matches!(format, ShowFormat::Pretty) {
        return Err("--follow is only supported with --pretty format".into());
    }

    let filters = filter.to_filters();
    let path = if let Some(session) = explicit_session.as_deref() {
        subcmd::resolve_session_ref(session, &home)?
    } else {
        subcmd::resolve_session(
            None,
            filter.query.as_deref(),
            &filters,
            &home,
            filter.search_mode(),
            filter.since_time()?,
            filter.until_time()?,
        )?
    };

    if let Some(fields) = meta_fields {
        let query = filter.query.clone().unwrap_or_default();
        // Validate -q early so a malformed regex doesn't silently turn into
        // an empty `matched` field — but only when `matched` was actually
        // requested. Other fields (`title`, `transcript`, …) don't depend
        // on the query, so an invalid `-q` shouldn't break them. Use the
        // engine `ResolveOpts::new` picks for this search_mode (bytes for
        // `all`, text for `prompt`) since their syntax differs.
        if !query.is_empty() && fields.contains(&crate::cli::Field::Matched) {
            match filter.search_mode() {
                crate::cli::SearchMode::All => regex::bytes::Regex::new(&format!("(?iu){}", query))
                    .map(drop)
                    .map_err(|e| format!("Invalid regex '{}': {}", query, e))?,
                crate::cli::SearchMode::Prompt => regex::Regex::new(&format!("(?i){}", query))
                    .map(drop)
                    .map_err(|e| format!("Invalid regex '{}': {}", query, e))?,
            }
        }
        return run_meta(&path, &home, &fields, &query, filter.search_mode());
    }

    // Validate --follow against plugin capability before displaying anything
    let plugin = agents::find_plugin_for_path(&path);
    if follow && !plugin.can_follow() {
        return Err(format!(
            "--follow is not supported for {} sessions",
            plugin.id()
        ));
    }

    match format {
        ShowFormat::Raw => {
            if let Ok(content) = std::fs::read_to_string(&path) {
                print!("{}", content);
            } else {
                return Err(format!("Failed to read: {}", path.display()));
            }
        }
        ShowFormat::Pretty => {
            let hl_re = match args.highlight.as_deref() {
                Some(p) => compile_highlight(p)?,
                None => None,
            };
            run_pretty(&path, args.head, hl_re.as_ref());
        }
        ShowFormat::Json => run_json(&path, args.head),
        ShowFormat::Md => run_md(&path, args.head),
        ShowFormat::Tsv => unreachable!("handled by meta_fields above"),
    }

    if follow {
        let hl_re = match args.highlight.as_deref() {
            Some(p) => compile_highlight(p)?,
            None => None,
        };
        run_follow(&path, plugin, hl_re.as_ref())?;
    }

    Ok(())
}

/// Output session metadata fields as TSV.
fn run_meta(
    path: &std::path::Path,
    home: &std::path::Path,
    fields: &[crate::cli::Field],
    query: &str,
    search_mode: crate::cli::SearchMode,
) -> Result<(), String> {
    emit_session_meta_tsv(path, home, fields, query, search_mode)
}

/// Resolve metadata fields for a single session and print them as TSV.
/// Shared by `show::run_meta` (`ah show -o ...`) and `fuzzy::print_session_fields`
/// (post-selection output for `ah log -i -o ...` / `ah show -i -o ...`) so the
/// two paths stay structurally identical (no truncation, query/search-mode
/// threading, running/pid enrichment, TSV escaping).
pub(crate) fn emit_session_meta_tsv(
    path: &std::path::Path,
    home: &std::path::Path,
    fields: &[crate::cli::Field],
    query: &str,
    search_mode: crate::cli::SearchMode,
) -> Result<(), String> {
    let plugin = agents::find_plugin_for_path(path);
    // Fail fast if the file is unreadable so scripts get a non-zero exit
    // instead of plausible-but-empty TSV values.
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("Failed to read session metadata: {}", e))?;
    // Pass query through to ResolveOpts so the `matched` field can populate
    // for `ah show -q QUERY -o matched`. transcript_limit/title_limit = 0 to
    // avoid truncating fields like `transcript`/`first_prompt`/`title` —
    // metadata mode is for scripting and should be lossless.
    let opts = resolver::ResolveOpts::new(query, 0, 0).with_search_mode(search_mode);
    // running/pid enrichment needs Id; resolve it alongside even if not asked.
    let needs_id = (fields.contains(&crate::cli::Field::Running)
        || fields.contains(&crate::cli::Field::Pid))
        && !fields.contains(&crate::cli::Field::Id);
    let mut resolve_set: Vec<crate::cli::Field> = fields.to_vec();
    if needs_id {
        resolve_set.push(crate::cli::Field::Id);
    }
    let mut resolved = resolver::resolve_fields(path, plugin, mtime, home, &resolve_set, &opts);
    // The pipeline path enriches `running`/`pid` from the live pid map; do
    // the same here so `ah show -o running,pid` returns truthful values.
    enrich_running_pid(&mut resolved, fields);
    // Escape `\t` / `\n` / `\r` in every field value (including
    // `Field::Path`) so multiline fields like `transcript`/`first_prompt`/
    // `matched` don't break TSV row/column boundaries. Backslashes pass
    // through verbatim — `escape_tsv` is intentionally non-symmetric so
    // non-path fields stay readable. Paths with literal tabs/newlines (rare
    // but legal on Unix) round-trip via `subcmd::resolve_session_ref`'s
    // raw-first / `unescape_tsv`-fallback ordering.
    let values: Vec<String> = fields
        .iter()
        .map(|f| crate::output::escape_tsv(resolved.get(f).map(|s| s.as_str()).unwrap_or("")))
        .collect();
    println!("{}", values.join("\t"));
    Ok(())
}

/// Inject the `running` / `pid` fields from the live PID map into a resolved
/// field set. Mirrors what `pipeline::run_pipeline` does for the log path so
/// metadata mode stays consistent.
pub(crate) fn enrich_running_pid(
    resolved: &mut std::collections::BTreeMap<crate::cli::Field, String>,
    requested: &[crate::cli::Field],
) {
    use crate::cli::Field;
    if !requested.contains(&Field::Running) && !requested.contains(&Field::Pid) {
        return;
    }
    let id = resolved.get(&Field::Id).cloned().unwrap_or_default();
    // Match the log/pipeline normalization: a session that the pid map
    // doesn't know about (no Id, archived, subagent, etc.) reports
    // running=false / pid="" rather than leaving the fields empty, so
    // `ah show -o running` is the same boolean shape as `ah log -o running`.
    let pid = if id.is_empty() {
        None
    } else {
        crate::build_pid_map().get(&id).copied()
    };
    if requested.contains(&Field::Running) {
        let v = if pid.is_some() { "true" } else { "false" };
        resolved.insert(Field::Running, v.to_string());
    }
    if requested.contains(&Field::Pid) {
        let v = pid.map(|p| p.to_string()).unwrap_or_default();
        resolved.insert(Field::Pid, v);
    }
}

fn run_pretty(path: &std::path::Path, head: Option<usize>, hl_re: Option<&regex::Regex>) {
    let plugin = agents::find_plugin_for_path(path);
    let is_tty = color::use_color();
    let limit = head.unwrap_or(0);

    let mut count: usize = 0;
    let mut first = true;
    plugin.iter_messages(path, &mut |message| {
        count += 1;
        if limit > 0 && count > limit {
            return false;
        }

        if !first {
            println!();
        }
        first = false;

        let text = strip_ansi(&message.text);
        let text = if is_tty {
            if let Some(re) = hl_re {
                highlight_text(&text, re)
            } else {
                text
            }
        } else {
            text
        };
        match message.role {
            MessageRole::User => {
                if is_tty {
                    println!("{}>>> {}{}", BOLD, text, RESET);
                } else {
                    println!(">>> {}", text);
                }
            }
            MessageRole::Assistant => {
                if is_tty {
                    print!("{}{}{}", DIM, text, RESET);
                } else {
                    print!("{}", text);
                }
                println!();
            }
        }
        true
    });

    if first {
        eprintln!("(only metadata — no conversation messages)");
        eprintln!();
        if let Ok(content) = std::fs::read_to_string(path) {
            print!("{}", content);
        }
    }
}

fn run_json(path: &std::path::Path, head: Option<usize>) {
    let plugin = agents::find_plugin_for_path(path);
    let limit = head.unwrap_or(0);

    let mut count: usize = 0;
    plugin.iter_messages(path, &mut |message| {
        count += 1;
        if limit > 0 && count > limit {
            return false;
        }

        let role = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };
        let obj = serde_json::json!({
            "role": role,
            "text": message.text,
        });
        println!("{}", obj);
        true
    });
}

fn run_md(path: &std::path::Path, head: Option<usize>) {
    let plugin = agents::find_plugin_for_path(path);
    let limit = head.unwrap_or(0);

    let mut count: usize = 0;
    let mut first = true;
    plugin.iter_messages(path, &mut |message| {
        count += 1;
        if limit > 0 && count > limit {
            return false;
        }

        if !first {
            println!();
            println!("---");
            println!();
        }
        first = false;

        let text = strip_ansi(&message.text);
        match message.role {
            MessageRole::User => {
                println!("## User");
                println!();
                println!("{}", text);
            }
            MessageRole::Assistant => {
                println!("## Assistant");
                println!();
                println!("{}", text);
            }
        }
        true
    });
}

/// Follow a session file for new messages (like tail -f).
/// Seeks to end of file and polls for new JSONL lines, printing them as pretty output.
fn run_follow(
    path: &std::path::Path,
    plugin: &dyn agents::AgentPlugin,
    hl_re: Option<&regex::Regex>,
) -> Result<(), String> {
    let is_tty = color::use_color();

    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Failed to open for follow: {}", e))?;
    // Seek to current end
    let mut pos = file
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("Failed to seek: {}", e))?;

    if is_tty {
        eprintln!(
            "{}--- following {} (Ctrl-C to stop) ---{}",
            DIM,
            path.display(),
            RESET
        );
    } else {
        eprintln!("--- following {} (Ctrl-C to stop) ---", path.display());
    }

    // Buffer for incomplete trailing lines across iterations
    let mut remainder = Vec::new();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(300));

        let meta = std::fs::metadata(path).map_err(|e| format!("Failed to stat: {}", e))?;
        let new_len = meta.len();
        if new_len < pos {
            // File was truncated/rotated — re-open and reset
            file = std::fs::File::open(path)
                .map_err(|e| format!("Failed to re-open after rotation: {}", e))?;
            pos = 0;
            remainder.clear();
        }
        if new_len <= pos {
            continue;
        }

        file.seek(SeekFrom::Start(pos))
            .map_err(|e| format!("Failed to seek: {}", e))?;

        // Read appended bytes in bounded chunks to cap memory usage.
        let chunk_size = ((new_len - pos) as usize).min(64 * 1024);
        let mut buf = vec![0u8; chunk_size];
        use std::io::Read;
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("Failed to read: {}", e))?;
        buf.truncate(n);
        pos += n as u64;

        // Prepend any leftover bytes from the previous iteration
        if !remainder.is_empty() {
            remainder.extend_from_slice(&buf);
            buf = std::mem::take(&mut remainder);
        }

        // If data doesn't end with newline, save trailing incomplete line for next iteration
        if !buf.is_empty() && buf[buf.len() - 1] != b'\n' {
            if let Some(last_nl) = memchr::memrchr(b'\n', &buf) {
                remainder = buf[last_nl + 1..].to_vec();
                buf.truncate(last_nl + 1);
            } else {
                // No newline at all — entire chunk is incomplete
                remainder = buf;
                continue;
            }
        }

        for line_bytes in buf.split(|&b| b == b'\n') {
            if line_bytes.len() < 2 {
                continue;
            }
            let line = match std::str::from_utf8(line_bytes) {
                Ok(l) => l,
                Err(_) => continue,
            };
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                for message in plugin.messages_from_value(&val) {
                    println!();
                    let text = strip_ansi(&message.text);
                    let text = if is_tty {
                        if let Some(re) = hl_re {
                            highlight_text(&text, re)
                        } else {
                            text
                        }
                    } else {
                        text
                    };
                    match message.role {
                        MessageRole::User => {
                            if is_tty {
                                println!("{}>>> {}{}", BOLD, text, RESET);
                            } else {
                                println!(">>> {}", text);
                            }
                        }
                        MessageRole::Assistant => {
                            if is_tty {
                                print!("{}{}{}", DIM, text, RESET);
                            } else {
                                print!("{}", text);
                            }
                            println!();
                        }
                    }
                }
            }
        }
    }
}

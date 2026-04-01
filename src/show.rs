use std::io::{Seek, SeekFrom};

use crate::agents::common::canonical_home;
use crate::agents::{self, MessageRole};
use crate::cli::{FilterArgs, ShowArgs, ShowFormat};
use crate::color::{self, BOLD, DIM, RESET};
use crate::output::strip_ansi;
use crate::resolver;
use crate::subcmd;

pub fn run(args: ShowArgs, filter: &FilterArgs) -> Result<(), String> {
    let home = canonical_home();
    let format = args.format();
    let follow = args.follow;
    let meta_fields = args.meta_fields()?;

    // Validate --follow early before any I/O or session resolution
    if follow && !matches!(format, ShowFormat::Pretty) {
        return Err("--follow is only supported with --pretty format".into());
    }

    let filters = filter.to_filters();
    let path = subcmd::resolve_session(
        args.session.as_deref(),
        filter.query.as_deref(),
        &filters,
        &home,
        filter.search_mode(),
        filter.since_time()?,
        filter.until_time()?,
    )?;

    if let Some(fields) = meta_fields {
        return run_meta(&path, &home, &fields);
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
        ShowFormat::Pretty => run_pretty(&path, args.head),
        ShowFormat::Json => run_json(&path, args.head),
        ShowFormat::Md => run_md(&path, args.head),
        ShowFormat::Tsv => unreachable!("handled by meta_fields above"),
    }

    if follow {
        run_follow(&path, plugin)?;
    }

    Ok(())
}

/// Output session metadata fields as TSV.
fn run_meta(
    path: &std::path::Path,
    home: &std::path::Path,
    fields: &[crate::cli::Field],
) -> Result<(), String> {
    let plugin = agents::find_plugin_for_path(path);
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let resolved = resolver::resolve_fields(path, plugin, mtime, home, fields, &Default::default());
    let values: Vec<&str> = fields
        .iter()
        .map(|f| resolved.get(f).map(|v| v.as_str()).unwrap_or(""))
        .collect();
    println!("{}", values.join("\t"));
    Ok(())
}

fn run_pretty(path: &std::path::Path, head: Option<usize>) {
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
fn run_follow(path: &std::path::Path, plugin: &dyn agents::AgentPlugin) -> Result<(), String> {
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

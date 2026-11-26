use regex::Regex;
use unicode_width::UnicodeWidthChar;

use crate::cli::{Field, MemoryField, OutputFormat, ProjectField};
use crate::color::{self, BLUE, BOLD, BOLD_YELLOW, CYAN, DIM, GREEN, MAGENTA, RESET, YELLOW};
use crate::session::Session;

/// Compare two optional field values, using numeric comparison when `numeric` is true.
pub fn compare_field_values(
    a: Option<&String>,
    b: Option<&String>,
    numeric: bool,
) -> std::cmp::Ordering {
    if numeric {
        let ia = a.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let ib = b.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        ia.cmp(&ib)
    } else {
        a.cmp(&b)
    }
}

/// Get the color for a field.
pub fn field_color(field: &Field) -> &'static str {
    match field {
        Field::Agent => CYAN,
        Field::Project => GREEN,
        Field::ModifiedAt | Field::CreatedAt => BLUE,
        Field::Title => "",
        Field::FirstPrompt | Field::LastPrompt => "",
        Field::Matched => YELLOW,
        Field::Path => MAGENTA,
        Field::Cwd => MAGENTA,
        Field::Turns | Field::Size => DIM,
        Field::Running => GREEN,
        Field::Pid => BOLD,
        Field::Id | Field::ResumeCmd => DIM,
        _ => "",
    }
}

/// Visible width of a string (CJK characters count as 2).
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Truncate string to fit within `max_width` display columns.
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let full_width = display_width(s);
    if full_width <= max_width {
        return s.to_string();
    }
    if max_width <= 2 {
        return ".".repeat(max_width);
    }
    let limit = max_width - 2;
    let mut width = 0;
    let mut result = String::new();
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > limit {
            break;
        }
        width += cw;
        result.push(c);
    }
    result.push_str("..");
    result
}

/// Pad string to `target_width` display columns with trailing spaces.
pub fn pad_to_width(s: &str, target_width: usize) -> String {
    let w = display_width(s);
    if w >= target_width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(target_width - w))
    }
}

/// Strip ANSI/OSC escape sequences and C0/C1 control characters from a string.
/// Preserves \n, \r, \t which are handled separately by callers.
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC sequence
            match chars.peek() {
                Some('[') => {
                    // CSI sequence: ESC [ ... <letter>
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch.is_ascii_alphabetic() || ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    // OSC, DCS, SOS, PM and APC strings: ESC <c> ... (ST = ESC\ or BEL)
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        if ch == '\x07' {
                            chars.next();
                            break;
                        }
                        if ch == '\x1b' {
                            chars.next();
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                Some(_) => {
                    // Other ESC sequences: ESC + one char
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        // C0 controls (except \n \r \t)
        if c as u32 <= 0x1f && c != '\n' && c != '\r' && c != '\t' {
            continue;
        }
        // C1 controls
        if (0x80..=0x9f).contains(&(c as u32)) {
            continue;
        }
        result.push(c);
    }
    result
}

/// [`strip_ansi`] for display: also drops Unicode bidirectional controls.
/// Not for values that are used again, such as session references, where
/// those characters can be part of a real path.
pub fn strip_for_display(s: &str) -> String {
    let mut out = strip_ansi(s);
    out.retain(|c| !is_bidi_control(c));
    out
}

/// Unicode bidirectional formatting characters (`Bidi_Control`). They
/// reorder how the surrounding text is displayed, so session data could use
/// them to make a path or command look different from what it is.
pub fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Sanitize untrusted text for multi-line terminal display: strip escape
/// sequences and control characters, normalize CRLF to LF and drop lone CRs
/// so a line cannot rewind the cursor and overwrite what precedes it.
/// LF and TAB are kept.
pub fn sanitize_multiline(s: &str) -> String {
    let stripped = strip_for_display(s);
    if !stripped.contains('\r') {
        return stripped;
    }
    stripped.replace("\r\n", "\n").replace('\r', "")
}

/// Whether stdout is rendered by a terminal: a TTY, the pager, or output
/// requested with `--color`/`AH_COLOR=1` (e.g. an fzf preview), whether or
/// not `NO_COLOR` disables the colors. Machine-readable formats strip control
/// characters from data only in this case and stay unchanged in pipes.
pub fn terminal_bound() -> bool {
    crate::pager::is_active()
        || std::io::IsTerminal::is_terminal(&std::io::stdout())
        || color::color_requested()
        || color::use_color()
}

/// Apply `escape` to a TSV/LTSV value, stripping escape sequences and
/// control characters first when `terminal` is set.
pub fn tsv_value(s: &str, terminal: bool, escape: fn(&str) -> String) -> String {
    if terminal {
        escape(&strip_for_display(s))
    } else {
        escape(s)
    }
}

/// Strip escape sequences from each column of an already-escaped TSV line,
/// so an unterminated sequence cannot swallow the tabs after it.
pub fn strip_ansi_tsv_line(line: &str) -> String {
    line.split('\t')
        .map(strip_for_display)
        .collect::<Vec<_>>()
        .join("\t")
}

/// Strip surrounding single or double quotes.
pub fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')))
    {
        return &s[1..s.len() - 1];
    }
    s
}

/// Sanitize a value for single-line display: strip control sequences,
/// then replace newlines/tabs with space.
pub fn sanitize_for_display(s: &str) -> String {
    let stripped = strip_for_display(s);
    stripped
        .replace('\n', " ")
        .replace('\r', "")
        .replace('\t', " ")
}

/// Compute column widths from data rows.
/// Returns a vec of widths, one per column.
pub fn compute_column_widths(rows: &[Vec<String>]) -> Vec<usize> {
    if rows.is_empty() {
        return Vec::new();
    }
    let ncols = rows[0].len();
    let mut widths = vec![0usize; ncols];
    for row in rows {
        for (i, val) in row.iter().enumerate() {
            let w = display_width(val);
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }
    widths
}

/// Format a row into a fixed-width column string.
/// Last column is not padded (fills remaining space).
pub fn format_columns(row: &[String], widths: &[usize], colors: &[&str]) -> String {
    let mut parts = Vec::new();
    for (i, val) in row.iter().enumerate() {
        let color = colors.get(i).copied().unwrap_or("");
        let formatted = if i == row.len() - 1 {
            val.clone()
        } else {
            let truncated = truncate_to_width(val, widths[i]);
            pad_to_width(&truncated, widths[i])
        };
        if !color.is_empty() {
            parts.push(format!("{}{}{}", color, formatted, RESET));
        } else {
            parts.push(formatted);
        }
    }
    parts.join("  ")
}

/// Output sessions in the requested format.
pub fn output_sessions(sessions: &[Session], fields: &[Field], format: &OutputFormat, query: &str) {
    match format {
        OutputFormat::Log => output_log(sessions, query),
        OutputFormat::Table => output_table(sessions, fields, query),
        OutputFormat::Tsv => output_tsv(sessions, fields, query),
        OutputFormat::Ltsv => output_ltsv(sessions, fields),
        OutputFormat::Json => output_jsonl(sessions, fields),
    }
}

/// Git-log style multi-line output.
fn output_log(sessions: &[Session], query: &str) {
    let is_tty = color::use_color();
    let highlight_re = if is_tty && !query.is_empty() {
        Regex::new(&format!("(?i){}", query)).ok()
    } else {
        None
    };

    for (i, session) in sessions.iter().enumerate() {
        if i > 0 {
            println!();
        }

        let id = &sanitize_for_display(session.fields.get(&Field::Id).map_or("", |v| v.as_str()));
        let agent =
            &sanitize_for_display(session.fields.get(&Field::Agent).map_or("", |v| v.as_str()));
        let model =
            &sanitize_for_display(session.fields.get(&Field::Model).map_or("", |v| v.as_str()));
        let project = &sanitize_for_display(
            session
                .fields
                .get(&Field::Project)
                .map_or("", |v| v.as_str()),
        );
        let cwd = &sanitize_for_display(session.fields.get(&Field::Cwd).map_or("", |v| v.as_str()));
        let created = &sanitize_for_display(
            session
                .fields
                .get(&Field::CreatedAt)
                .map_or("", |v| v.as_str()),
        );
        let modified = &sanitize_for_display(
            session
                .fields
                .get(&Field::ModifiedAt)
                .map_or("", |v| v.as_str()),
        );
        let title = session.fields.get(&Field::Title).map_or("", |v| v.as_str());
        let prompt = session
            .fields
            .get(&Field::FirstPrompt)
            .map_or("", |v| v.as_str());
        let is_running = session
            .fields
            .get(&Field::Running)
            .is_some_and(|v| v == "true");

        if is_tty {
            let running_marker = if is_running {
                format!(" {}(running){}", GREEN, RESET)
            } else {
                String::new()
            };
            println!("{}session {}{}{}", YELLOW, id, RESET, running_marker);
            println!("Agent:    {}", agent);
            if !model.is_empty() {
                println!("Model:    {}", model);
            }
            println!("Project:  {}", project);
            if !cwd.is_empty() {
                println!("Cwd:      {}", cwd);
            }
            println!("Date:     {} - {}", created, modified);
        } else {
            let running_marker = if is_running { " (running)" } else { "" };
            println!("session {}{}", id, running_marker);
            println!("Agent:    {}", agent);
            if !model.is_empty() {
                println!("Model:    {}", model);
            }
            println!("Project:  {}", project);
            if !cwd.is_empty() {
                println!("Cwd:      {}", cwd);
            }
            println!("Date:     {} - {}", created, modified);
        }

        // Title (like git commit subject line, first line only)
        let title_line = &sanitize_for_display(title.lines().next().unwrap_or(title));
        if !title_line.is_empty() {
            println!();
            println!("    {}", title_line);
        }

        // First prompt (like git commit body, up to 5 lines)
        // Skip if prompt adds nothing beyond the title
        let prompt_first_line = prompt.lines().next().unwrap_or("");
        let title_base = title_line.strip_suffix("..").unwrap_or(title_line.as_str());
        let prompt_redundant = prompt.is_empty()
            || prompt == title_line.as_str()
            || prompt_first_line.starts_with(title_base);
        if !prompt_redundant {
            println!();
            let prompt_text = sanitize_multiline(prompt);
            let lines: Vec<&str> = prompt_text.lines().collect();
            let truncated = lines.len() > 5;
            for line in lines.iter().take(5) {
                println!("    {}", line);
            }
            if truncated {
                if is_tty {
                    println!("    {}...{}", DIM, RESET);
                } else {
                    println!("    ...");
                }
            }
        }

        // Matched excerpt when searching
        let matched = session
            .fields
            .get(&Field::Matched)
            .map_or("", |v| v.as_str());
        if !matched.is_empty() {
            println!();
            let matched_text = if let Some(ref re) = highlight_re {
                highlight_matches(&sanitize_for_display(matched), re)
            } else {
                sanitize_for_display(matched)
            };
            if is_tty {
                println!("    {}Match:{} {}", DIM, RESET, matched_text);
            } else {
                println!("    Match: {}", matched_text);
            }
        }
    }
}

/// Highlight matching portions in text with ANSI bold yellow.
fn highlight_matches(text: &str, re: &Regex) -> String {
    re.replace_all(text, |caps: &regex::Captures| {
        format!("{}{}{}", BOLD_YELLOW, &caps[0], RESET)
    })
    .into_owned()
}

/// Colorize a field value.
fn colorize_field(field: &Field, val: &str) -> String {
    color::colorize(field_color(field), val)
}

/// Table output (fixed-width columns for TTY).
fn output_table(sessions: &[Session], fields: &[Field], query: &str) {
    let has_matched = fields.contains(&Field::Matched);
    let is_tty = color::use_color();
    let highlight_re = if has_matched && is_tty && !query.is_empty() {
        Regex::new(&format!("(?i){}", query)).ok()
    } else {
        None
    };

    // Check if any session is running
    let any_running = sessions
        .iter()
        .any(|s| s.fields.get(&Field::Running).is_some_and(|v| v == "true"));

    // Running field is shown as R marker prefix, not as a column
    let display_fields: Vec<&Field> = fields.iter().filter(|f| **f != Field::Running).collect();

    // Build rows of sanitized values
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|session| {
            display_fields
                .iter()
                .map(|f| {
                    let val = session.fields.get(f).map_or("", |v| v.as_str());
                    sanitize_for_display(val)
                })
                .collect()
        })
        .collect();

    // Header row (field names in UPPER_CASE)
    let header: Vec<String> = display_fields
        .iter()
        .map(|f| f.name().to_uppercase())
        .collect();

    // Compute widths including header
    let mut all_rows = vec![header.clone()];
    all_rows.extend(rows.iter().cloned());
    let widths = compute_column_widths(&all_rows);
    let colors: Vec<&str> = display_fields.iter().map(|f| field_color(f)).collect();

    // Print header
    let header_prefix = if any_running { "  " } else { "" };
    if is_tty {
        println!(
            "{}{}{}{}",
            header_prefix,
            DIM,
            format_columns(&header, &widths, &[]),
            RESET
        );
    } else {
        println!("{}{}", header_prefix, format_columns(&header, &widths, &[]));
    }

    for (i, row) in rows.iter().enumerate() {
        // Running marker prefix
        let marker = if any_running {
            let is_running = sessions[i]
                .fields
                .get(&Field::Running)
                .is_some_and(|v| v == "true");
            if is_running {
                if is_tty {
                    format!("{}R{} ", GREEN, RESET)
                } else {
                    "R ".to_string()
                }
            } else {
                "  ".to_string()
            }
        } else {
            String::new()
        };
        if has_matched {
            let mut colored_row = Vec::new();
            for (j, val) in row.iter().enumerate() {
                let color = if is_tty {
                    colors.get(j).copied().unwrap_or("")
                } else {
                    ""
                };
                let formatted = if j == row.len() - 1 {
                    val.clone()
                } else {
                    let truncated = truncate_to_width(val, widths[j]);
                    pad_to_width(&truncated, widths[j])
                };
                if *display_fields[j] == Field::Matched {
                    if let Some(ref re) = highlight_re {
                        colored_row.push(highlight_matches(&formatted, re));
                    } else if is_tty {
                        colored_row.push(colorize_field(display_fields[j], &formatted));
                    } else {
                        colored_row.push(formatted);
                    }
                } else if !color.is_empty() {
                    colored_row.push(format!("{}{}{}", color, formatted, RESET));
                } else {
                    colored_row.push(formatted);
                }
            }
            println!("{}{}", marker, colored_row.join("  "));
        } else if is_tty {
            println!("{}{}", marker, format_columns(row, &widths, &colors));
        } else {
            println!("{}{}", marker, format_columns(row, &widths, &[]));
        }
    }
}

/// TSV output.
fn output_tsv(sessions: &[Session], fields: &[Field], query: &str) {
    let terminal = terminal_bound();
    let has_matched = fields.contains(&Field::Matched);
    let is_tty = color::use_color();
    let highlight_re = if has_matched && is_tty && !query.is_empty() {
        Regex::new(&format!("(?i){}", query)).ok()
    } else {
        None
    };

    for session in sessions {
        let values: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = session.fields.get(f).map_or("", |v| v.as_str());
                let escaped = tsv_value(val, terminal, escape_tsv);
                if is_tty {
                    if *f == Field::Matched
                        && let Some(ref re) = highlight_re
                    {
                        return highlight_matches(&escaped, re);
                    }
                    colorize_field(f, &escaped)
                } else {
                    escaped
                }
            })
            .collect();
        println!("{}", values.join("\t"));
    }
}

/// Escape characters that would break TSV row/column boundaries. Backslashes
/// pass through unchanged so non-path fields (`title`, `transcript`,
/// `matched`, …) appear verbatim to external tools. The path round-trip via
/// pipes is handled by `subcmd::resolve_session_ref`'s raw-first /
/// `unescape_tsv` fallback ordering — paths with literal `\t` / `\n` / `\r`
/// in the filename (extremely rare) win the literal pass before the decode
/// fallback runs.
///
/// Not symmetric with `unescape_tsv`: it cannot disambiguate a literal
/// `\t`/`\n`/`\r` source from a real TAB/LF/CR after round-trip. Use
/// [`escape_tsv_lossless`] when the same value will be unconditionally
/// `unescape_tsv`'d (e.g. fzf LTSV interactive selection in `fuzzy.rs`).
pub fn escape_tsv(s: &str) -> String {
    s.replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Symmetric escape: also doubles backslashes so `unescape_tsv(escape_tsv_lossless(s)) == s`
/// for any `s`. Used by interactive LTSV input/output in `fuzzy.rs` where a
/// captured value is unconditionally decoded again after fzf selection — a
/// non-symmetric encoder would corrupt paths containing literal `\t`/`\n`/
/// `\r` sequences (e.g. filenames whose name happens to contain backslash+t).
pub fn escape_tsv_lossless(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Decodes `\\` / `\t` / `\n` / `\r` back to `\` / TAB / LF / CR.
/// True inverse of [`escape_tsv_lossless`]; for [`escape_tsv`] it is a
/// best-effort decoder used as a fallback in `subcmd::resolve_session_ref`
/// (literal-path attempt first, then unescape).
pub fn unescape_tsv(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// LTSV output (Labeled Tab-Separated Values).
fn output_ltsv(sessions: &[Session], fields: &[Field]) {
    let terminal = terminal_bound();
    for session in sessions {
        let pairs: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = session.fields.get(f).map_or("", |v| v.as_str());
                format!("{}:{}", f.name(), tsv_value(val, terminal, escape_tsv))
            })
            .collect();
        println!("{}", pairs.join("\t"));
    }
}

/// JSON Lines output.
/// One session as a JSON object with the requested fields. List-valued
/// fields (`prompts`, `responses`, `messages`) are emitted as JSON arrays.
pub fn session_to_json(
    session: &Session,
    fields: &[Field],
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for field in fields {
        let val = session.fields.get(field).cloned().unwrap_or_default();
        if matches!(*field, Field::Prompts | Field::Responses | Field::Messages)
            && let Ok(arr) = serde_json::from_str::<serde_json::Value>(&val)
        {
            map.insert(field.name().to_string(), arr);
            continue;
        }
        map.insert(field.name().to_string(), serde_json::Value::String(val));
    }
    map
}

fn output_jsonl(sessions: &[Session], fields: &[Field]) {
    for session in sessions {
        if let Ok(json) = serde_json::to_string(&session_to_json(session, fields)) {
            println!("{}", json);
        }
    }
}

/// Get the color for a project field.
pub fn project_field_color(field: &ProjectField) -> &'static str {
    match field {
        ProjectField::Project => GREEN,
        ProjectField::ProjectRaw => DIM,
        ProjectField::Cwd => MAGENTA,
        ProjectField::SessionCount => BOLD,
        ProjectField::Sessions => DIM,
        ProjectField::Agents => CYAN,
        ProjectField::LastModifiedAt | ProjectField::FirstCreatedAt => BLUE,
    }
}

/// Colorize a project field value.
fn colorize_project(field: &ProjectField, val: &str) -> String {
    color::colorize(project_field_color(field), val)
}

/// Output project records in the requested format.
pub fn output_projects(
    records: &[std::collections::BTreeMap<ProjectField, String>],
    fields: &[ProjectField],
    format: &OutputFormat,
) {
    match format {
        OutputFormat::Log | OutputFormat::Table => output_projects_table(records, fields),
        OutputFormat::Tsv => output_projects_tsv(records, fields),
        OutputFormat::Ltsv => output_projects_ltsv(records, fields),
        OutputFormat::Json => output_projects_jsonl(records, fields),
    }
}

fn output_projects_table(
    records: &[std::collections::BTreeMap<ProjectField, String>],
    fields: &[ProjectField],
) {
    let is_tty = color::use_color();

    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|record| {
            fields
                .iter()
                .map(|f| {
                    let val = record.get(f).map_or("", |v| v.as_str());
                    sanitize_for_display(val)
                })
                .collect()
        })
        .collect();

    let header: Vec<String> = fields.iter().map(|f| f.name().to_uppercase()).collect();
    let mut all_rows = vec![header.clone()];
    all_rows.extend(rows.iter().cloned());
    let widths = compute_column_widths(&all_rows);
    let colors: Vec<&str> = if is_tty {
        fields.iter().map(|f| project_field_color(f)).collect()
    } else {
        Vec::new()
    };

    if is_tty {
        println!("{}{}{}", DIM, format_columns(&header, &widths, &[]), RESET);
    } else {
        println!("{}", format_columns(&header, &widths, &[]));
    }

    for row in &rows {
        println!("{}", format_columns(row, &widths, &colors));
    }
}

fn output_projects_tsv(
    records: &[std::collections::BTreeMap<ProjectField, String>],
    fields: &[ProjectField],
) {
    let terminal = terminal_bound();
    let is_tty = color::use_color();
    for record in records {
        let values: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = record.get(f).map_or("", |v| v.as_str());
                let escaped = tsv_value(val, terminal, escape_tsv);
                if is_tty {
                    colorize_project(f, &escaped)
                } else {
                    escaped
                }
            })
            .collect();
        println!("{}", values.join("\t"));
    }
}

fn output_projects_ltsv(
    records: &[std::collections::BTreeMap<ProjectField, String>],
    fields: &[ProjectField],
) {
    let terminal = terminal_bound();
    for record in records {
        let pairs: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = record.get(f).map_or("", |v| v.as_str());
                format!("{}:{}", f.name(), tsv_value(val, terminal, escape_tsv))
            })
            .collect();
        println!("{}", pairs.join("\t"));
    }
}

/// One project record as a JSON object (`sessions` as an array,
/// `session_count` as a number).
pub fn project_to_json(
    record: &std::collections::BTreeMap<ProjectField, String>,
    fields: &[ProjectField],
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for field in fields {
        let val = record.get(field).cloned().unwrap_or_default();
        // sessions as JSON array
        if *field == ProjectField::Sessions
            && let Ok(arr) = serde_json::from_str::<serde_json::Value>(&val)
        {
            map.insert(field.name().to_string(), arr);
            continue;
        }
        // session_count as number
        if *field == ProjectField::SessionCount
            && let Ok(n) = val.parse::<u64>()
        {
            map.insert(
                field.name().to_string(),
                serde_json::Value::Number(n.into()),
            );
            continue;
        }
        map.insert(field.name().to_string(), serde_json::Value::String(val));
    }
    map
}

fn output_projects_jsonl(
    records: &[std::collections::BTreeMap<ProjectField, String>],
    fields: &[ProjectField],
) {
    for record in records {
        if let Ok(json) = serde_json::to_string(&project_to_json(record, fields)) {
            println!("{}", json);
        }
    }
}

/// Get the color for a memory field.
pub fn memory_field_color(field: &MemoryField) -> &'static str {
    match field {
        MemoryField::Agent => CYAN,
        MemoryField::Project => GREEN,
        MemoryField::Type => BOLD,
        MemoryField::Name => "",
        MemoryField::Description => DIM,
        MemoryField::ModifiedAt | MemoryField::CreatedAt => BLUE,
        MemoryField::Size | MemoryField::Lines => DIM,
        MemoryField::FileName => "",
        MemoryField::Path => MAGENTA,
        MemoryField::Body => "",
        MemoryField::Matched => YELLOW,
    }
}

/// Colorize a memory field value.
fn colorize_memory(field: &MemoryField, val: &str) -> String {
    color::colorize(memory_field_color(field), val)
}

/// Output memory records in the requested format.
pub fn output_memory(
    records: &[std::collections::BTreeMap<MemoryField, String>],
    fields: &[MemoryField],
    format: &OutputFormat,
    query: &str,
) {
    match format {
        OutputFormat::Log | OutputFormat::Table => output_memory_table(records, fields, query),
        OutputFormat::Tsv => output_memory_tsv(records, fields, query),
        OutputFormat::Ltsv => output_memory_ltsv(records, fields),
        OutputFormat::Json => output_memory_jsonl(records, fields),
    }
}

fn output_memory_table(
    records: &[std::collections::BTreeMap<MemoryField, String>],
    fields: &[MemoryField],
    query: &str,
) {
    let is_tty = color::use_color();
    let highlight_re = if is_tty && !query.is_empty() {
        Regex::new(&format!("(?i){}", query)).ok()
    } else {
        None
    };

    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|record| {
            fields
                .iter()
                .map(|f| {
                    let val = record.get(f).map_or("", |v| v.as_str());
                    sanitize_for_display(val)
                })
                .collect()
        })
        .collect();

    let header: Vec<String> = fields.iter().map(|f| f.name().to_uppercase()).collect();
    let mut all_rows = vec![header.clone()];
    all_rows.extend(rows.iter().cloned());
    let widths = compute_column_widths(&all_rows);
    let colors: Vec<&str> = if is_tty {
        fields.iter().map(|f| memory_field_color(f)).collect()
    } else {
        Vec::new()
    };

    if is_tty {
        println!("{}{}{}", DIM, format_columns(&header, &widths, &[]), RESET);
    } else {
        println!("{}", format_columns(&header, &widths, &[]));
    }

    for row in &rows {
        let formatted = format_columns(row, &widths, &colors);
        if let Some(ref re) = highlight_re {
            println!("{}", highlight_matches(&formatted, re));
        } else {
            println!("{}", formatted);
        }
    }
}

fn output_memory_tsv(
    records: &[std::collections::BTreeMap<MemoryField, String>],
    fields: &[MemoryField],
    query: &str,
) {
    let terminal = terminal_bound();
    let is_tty = color::use_color();
    let highlight_re = if is_tty && !query.is_empty() {
        Regex::new(&format!("(?i){}", query)).ok()
    } else {
        None
    };

    for record in records {
        let values: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = record.get(f).map_or("", |v| v.as_str());
                let escaped = tsv_value(val, terminal, escape_tsv);
                if is_tty {
                    colorize_memory(f, &escaped)
                } else {
                    escaped
                }
            })
            .collect();
        let line = values.join("\t");
        if let Some(ref re) = highlight_re {
            println!("{}", highlight_matches(&line, re));
        } else {
            println!("{}", line);
        }
    }
}

fn output_memory_ltsv(
    records: &[std::collections::BTreeMap<MemoryField, String>],
    fields: &[MemoryField],
) {
    let terminal = terminal_bound();
    for record in records {
        let pairs: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = record.get(f).map_or("", |v| v.as_str());
                format!("{}:{}", f.name(), tsv_value(val, terminal, escape_tsv))
            })
            .collect();
        println!("{}", pairs.join("\t"));
    }
}

/// One memory record as a JSON object of string fields.
pub fn memory_to_json(
    record: &std::collections::BTreeMap<MemoryField, String>,
    fields: &[MemoryField],
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for field in fields {
        let val = record.get(field).cloned().unwrap_or_default();
        map.insert(field.name().to_string(), serde_json::Value::String(val));
    }
    map
}

fn output_memory_jsonl(
    records: &[std::collections::BTreeMap<MemoryField, String>],
    fields: &[MemoryField],
) {
    for record in records {
        if let Ok(json) = serde_json::to_string(&memory_to_json(record, fields)) {
            println!("{}", json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn make_session(fields: Vec<(Field, &str)>) -> Session {
        let mut map = BTreeMap::new();
        for (f, v) in fields {
            map.insert(f, v.to_string());
        }
        Session {
            path: PathBuf::from("/tmp/test.jsonl"),
            fields: map,
        }
    }

    #[test]
    fn test_output_tsv_format() {
        let sessions = vec![make_session(vec![
            (Field::Agent, "claude"),
            (Field::Project, "myapp"),
        ])];
        let fields = vec![Field::Agent, Field::Project];
        output_tsv(&sessions, &fields, "");
    }

    #[test]
    fn test_output_ltsv_format() {
        let sessions = vec![make_session(vec![
            (Field::Agent, "claude"),
            (Field::Project, "myapp"),
        ])];
        let fields = vec![Field::Agent, Field::Project];
        output_ltsv(&sessions, &fields);
    }

    #[test]
    fn test_escape_tsv() {
        assert_eq!(escape_tsv("hello"), "hello");
        assert_eq!(escape_tsv("a\tb"), "a\\tb");
        assert_eq!(escape_tsv("a\nb"), "a\\nb");
        assert_eq!(escape_tsv("a\r\nb"), "a\\r\\nb");
        // Backslashes pass through unchanged so non-path fields (title,
        // transcript, …) appear verbatim to external consumers.
        assert_eq!(escape_tsv("C:\\Users\\foo"), "C:\\Users\\foo");
        assert_eq!(escape_tsv("line1\nline2\tcol"), "line1\\nline2\\tcol");
    }

    #[test]
    fn test_escape_tsv_lossless_round_trip() {
        // The lossless variant must be a true inverse of unescape_tsv for
        // every input — including paths that look like escape sequences in
        // their literal form (e.g. `/tmp/ah\tcase` where `\t` is two chars).
        for s in [
            "hello",
            "a\tb",   // real TAB
            "a\nb",   // real LF
            "a\r\nb", // real CRLF
            "a\\b",   // single backslash
            "C:\\Users\\foo",
            "/tmp/ah\\tcase/x", // literal `\t` in filename
            "/path\\with\nmix\tof\\\\stuff",
        ] {
            let encoded = escape_tsv_lossless(s);
            let decoded = unescape_tsv(&encoded);
            assert_eq!(
                decoded, s,
                "round-trip failed: {:?} -> {:?} -> {:?}",
                s, encoded, decoded
            );
        }
    }

    #[test]
    fn test_unescape_tsv_basic() {
        assert_eq!(unescape_tsv("hello"), "hello");
        assert_eq!(unescape_tsv("a\\tb"), "a\tb");
        assert_eq!(unescape_tsv("a\\nb"), "a\nb");
        assert_eq!(unescape_tsv("a\\r\\nb"), "a\r\nb");
        assert_eq!(unescape_tsv("a\\\\b"), "a\\b");
        // Unknown escape sequence: backslash kept verbatim.
        assert_eq!(unescape_tsv("a\\xb"), "a\\xb");
        // Trailing lone backslash: kept.
        assert_eq!(unescape_tsv("a\\"), "a\\");
    }

    #[test]
    fn test_output_jsonl_prompts_as_array() {
        let sessions = vec![make_session(vec![
            (Field::Title, "fix-bug"),
            (Field::Prompts, r#"["hello","world"]"#),
        ])];
        let fields = vec![Field::Title, Field::Prompts];
        output_jsonl(&sessions, &fields);
    }

    #[test]
    fn test_strip_ansi_plain_text() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn test_strip_ansi_csi_sequence() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1;34mbold blue\x1b[0m"), "bold blue");
    }

    #[test]
    fn test_strip_ansi_osc_sequence() {
        // OSC with BEL terminator
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
        // OSC with ST terminator
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\text"), "text");
    }

    #[test]
    fn test_strip_ansi_c0_controls() {
        // NUL, BEL, BS should be stripped; \n \r \t preserved
        assert_eq!(strip_ansi("a\x00b\x07c\x08d"), "abcd");
        assert_eq!(strip_ansi("a\nb\rc\td"), "a\nb\rc\td");
    }

    #[test]
    fn test_strip_ansi_c1_controls() {
        assert_eq!(strip_ansi("a\u{0080}b\u{009f}c"), "abc");
    }

    #[test]
    fn test_strip_ansi_string_sequences() {
        // DCS, SOS, PM and APC payloads are dropped with their ST/BEL terminator.
        assert_eq!(strip_ansi("a\x1bPq#0;2;0;0;0\x1b\\b"), "ab");
        assert_eq!(strip_ansi("a\x1b_payload\x07b"), "ab");
        assert_eq!(strip_ansi("a\x1b^pm\x1b\\b\x1bXsos\x1b\\c"), "abc");
    }

    #[test]
    fn test_sanitize_multiline_strips_terminal_controls() {
        let input = concat!(
            "\x1b[31mred\x1b[0m ",
            "\x1b]8;;https://evil.example\x07link\x1b]8;;\x07 ",
            "\x1b]52;c;QUJD\x07",
            "\x1b]52;c;QUJD\x1b\\",
            "\u{9b}2J\u{9d}0;t",
            "\x07\x08\x00end",
        );
        let out = sanitize_multiline(input);
        assert_eq!(out, "red link 2J0;tend");
        for c in ['\x1b', '\x07', '\x08', '\0', '\u{9b}', '\u{9d}'] {
            assert!(!out.contains(c), "{c:?} left in {out:?}");
        }
    }

    #[test]
    fn test_sanitize_multiline_line_endings() {
        assert_eq!(sanitize_multiline("a\r\nb\r\n"), "a\nb\n");
        // A lone CR must not rewind the cursor over the start of the line.
        assert_eq!(sanitize_multiline("safe\rFAKE"), "safeFAKE");
        assert_eq!(sanitize_multiline("a\r\r\nb"), "a\nb");
        assert_eq!(sanitize_multiline("tab\tkept"), "tab\tkept");
    }

    #[test]
    fn test_sanitize_multiline_keeps_text_and_markdown() {
        let md = "## 見出し\n\n- **太字** と `code`\n\n```rust\nfn main() {}\n```\n";
        assert_eq!(sanitize_multiline(md), md);
    }

    #[test]
    fn test_tsv_value_strips_only_for_terminal() {
        let v = "a\x1b]52;c;QUJD\x07b\tc";
        assert_eq!(tsv_value(v, false, escape_tsv), "a\x1b]52;c;QUJD\x07b\\tc");
        assert_eq!(tsv_value(v, true, escape_tsv), "ab\\tc");
        assert_eq!(
            tsv_value("x\x1b[1m\\y", true, escape_tsv_lossless),
            "x\\\\y"
        );
    }

    #[test]
    fn test_strip_ansi_bidi_controls() {
        assert_eq!(
            strip_for_display("run \u{202E}txt.exe\u{202C} \u{2066}x\u{2069}\u{200F}\u{061C}"),
            "run txt.exe x"
        );
        assert_eq!(strip_for_display("שלום مرحبا"), "שלום مرحبا");
        assert_eq!(sanitize_multiline("a\u{202E}b"), "ab");
        assert_eq!(sanitize_for_display("a\u{2066}b"), "ab");
        assert_eq!(tsv_value("a\u{202E}b", true, escape_tsv), "ab");
        // Values used again, such as session references, keep them.
        assert_eq!(strip_ansi("bidi-\u{202E}.jsonl"), "bidi-\u{202E}.jsonl");
    }

    #[test]
    fn test_strip_ansi_tsv_line_keeps_columns() {
        let line = "path\tA\x1b]8;;unterminated\tcodex\tB\x1b[31m";
        assert_eq!(strip_ansi_tsv_line(line), "path\tA\tcodex\tB");
        // Joining first would drop the later columns.
        assert_eq!(strip_ansi(line).split('\t').count(), 2);
    }

    #[test]
    fn test_sanitize_for_display_with_ansi() {
        assert_eq!(
            sanitize_for_display("\x1b[31mred\x1b[0m\nline2"),
            "red line2"
        );
    }
}

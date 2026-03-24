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
                Some(']') => {
                    // OSC sequence: ESC ] ... (ST = ESC\ or BEL)
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
    let stripped = strip_ansi(s);
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

        let id = session
            .fields
            .get(&Field::Id)
            .map(|v| v.as_str())
            .unwrap_or("");
        let agent = session
            .fields
            .get(&Field::Agent)
            .map(|v| v.as_str())
            .unwrap_or("");
        let project = session
            .fields
            .get(&Field::Project)
            .map(|v| v.as_str())
            .unwrap_or("");
        let cwd = session
            .fields
            .get(&Field::Cwd)
            .map(|v| v.as_str())
            .unwrap_or("");
        let created = session
            .fields
            .get(&Field::CreatedAt)
            .map(|v| v.as_str())
            .unwrap_or("");
        let modified = session
            .fields
            .get(&Field::ModifiedAt)
            .map(|v| v.as_str())
            .unwrap_or("");
        let title = session
            .fields
            .get(&Field::Title)
            .map(|v| v.as_str())
            .unwrap_or("");
        let prompt = session
            .fields
            .get(&Field::FirstPrompt)
            .map(|v| v.as_str())
            .unwrap_or("");
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
            println!("Project:  {}", project);
            if !cwd.is_empty() {
                println!("Cwd:      {}", cwd);
            }
            println!("Date:     {} - {}", created, modified);
        } else {
            let running_marker = if is_running { " (running)" } else { "" };
            println!("session {}{}", id, running_marker);
            println!("Agent:    {}", agent);
            println!("Project:  {}", project);
            if !cwd.is_empty() {
                println!("Cwd:      {}", cwd);
            }
            println!("Date:     {} - {}", created, modified);
        }

        // Title (like git commit subject line, first line only)
        let title_line = title.lines().next().unwrap_or(title);
        if !title_line.is_empty() {
            println!();
            println!("    {}", title_line);
        }

        // First prompt (like git commit body, up to 5 lines)
        // Skip if prompt adds nothing beyond the title
        let prompt_first_line = prompt.lines().next().unwrap_or("");
        let title_base = title_line.strip_suffix("..").unwrap_or(title_line);
        let prompt_redundant =
            prompt.is_empty() || prompt == title_line || prompt_first_line.starts_with(title_base);
        if !prompt_redundant {
            println!();
            let prompt_text = strip_ansi(prompt);
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
            .map(|v| v.as_str())
            .unwrap_or("");
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
                    let val = session.fields.get(f).map(|v| v.as_str()).unwrap_or("");
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
                let val = session.fields.get(f).map(|v| v.as_str()).unwrap_or("");
                let escaped = escape_tsv(val);
                if is_tty {
                    if *f == Field::Matched {
                        if let Some(ref re) = highlight_re {
                            return highlight_matches(&escaped, re);
                        }
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

/// Escape newlines and tabs for TSV/LTSV output.
pub fn escape_tsv(s: &str) -> String {
    s.replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// LTSV output (Labeled Tab-Separated Values).
fn output_ltsv(sessions: &[Session], fields: &[Field]) {
    for session in sessions {
        let pairs: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = session.fields.get(f).map(|v| v.as_str()).unwrap_or("");
                format!("{}:{}", f.name(), escape_tsv(val))
            })
            .collect();
        println!("{}", pairs.join("\t"));
    }
}

/// JSON Lines output.
fn output_jsonl(sessions: &[Session], fields: &[Field]) {
    for session in sessions {
        let mut map = serde_json::Map::new();
        for field in fields {
            let val = session.fields.get(field).cloned().unwrap_or_default();
            if matches!(*field, Field::Prompts | Field::Responses | Field::Messages) {
                if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&val) {
                    map.insert(field.name().to_string(), arr);
                    continue;
                }
            }
            map.insert(field.name().to_string(), serde_json::Value::String(val));
        }
        if let Ok(json) = serde_json::to_string(&map) {
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
                    let val = record.get(f).map(|v| v.as_str()).unwrap_or("");
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
    let is_tty = color::use_color();
    for record in records {
        let values: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = record.get(f).map(|v| v.as_str()).unwrap_or("");
                let escaped = escape_tsv(val);
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
    for record in records {
        let pairs: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = record.get(f).map(|v| v.as_str()).unwrap_or("");
                format!("{}:{}", f.name(), escape_tsv(val))
            })
            .collect();
        println!("{}", pairs.join("\t"));
    }
}

fn output_projects_jsonl(
    records: &[std::collections::BTreeMap<ProjectField, String>],
    fields: &[ProjectField],
) {
    for record in records {
        let mut map = serde_json::Map::new();
        for field in fields {
            let val = record.get(field).cloned().unwrap_or_default();
            // sessions as JSON array
            if *field == ProjectField::Sessions {
                if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&val) {
                    map.insert(field.name().to_string(), arr);
                    continue;
                }
            }
            // session_count as number
            if *field == ProjectField::SessionCount {
                if let Ok(n) = val.parse::<u64>() {
                    map.insert(
                        field.name().to_string(),
                        serde_json::Value::Number(n.into()),
                    );
                    continue;
                }
            }
            map.insert(field.name().to_string(), serde_json::Value::String(val));
        }
        if let Ok(json) = serde_json::to_string(&map) {
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
                    let val = record.get(f).map(|v| v.as_str()).unwrap_or("");
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
                let val = record.get(f).map(|v| v.as_str()).unwrap_or("");
                let escaped = escape_tsv(val);
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
    for record in records {
        let pairs: Vec<String> = fields
            .iter()
            .map(|f| {
                let val = record.get(f).map(|v| v.as_str()).unwrap_or("");
                format!("{}:{}", f.name(), escape_tsv(val))
            })
            .collect();
        println!("{}", pairs.join("\t"));
    }
}

fn output_memory_jsonl(
    records: &[std::collections::BTreeMap<MemoryField, String>],
    fields: &[MemoryField],
) {
    for record in records {
        let mut map = serde_json::Map::new();
        for field in fields {
            let val = record.get(field).cloned().unwrap_or_default();
            map.insert(field.name().to_string(), serde_json::Value::String(val));
        }
        if let Ok(json) = serde_json::to_string(&map) {
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
        assert_eq!(escape_tsv("a\\b"), "a\\b");
        assert_eq!(escape_tsv("line1\nline2\tcol"), "line1\\nline2\\tcol");
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
    fn test_sanitize_for_display_with_ansi() {
        assert_eq!(
            sanitize_for_display("\x1b[31mred\x1b[0m\nline2"),
            "red line2"
        );
    }
}

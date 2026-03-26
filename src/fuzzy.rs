use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::agents::common::canonical_home;
use crate::cli::{
    Field, FilterArgs, InteractiveArgs, ListProjectsArgs, ListProjectsResolvedArgs, MemoryArgs,
    MemoryField, MemoryResolvedArgs, ProjectField, ResumeArgs, SearchArgs, ShowArgs, SortOrder,
};
use crate::memory;
use crate::output::{self, compute_column_widths, format_columns, sanitize_for_display};
use crate::pipeline;
use crate::projects;
use crate::resolver::{self, shell_quote};

const PREVIEW_SELECTORS: &[&str] = &["fzf", "sk"];

/// Reverse shell_quote: strip surrounding single quotes and unescape.
fn strip_shell_quote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].replace("'\"'\"'", "'")
    } else {
        s.to_string()
    }
}

fn resolve_selector(ia: &InteractiveArgs) -> String {
    ia.selector
        .clone()
        .or_else(|| std::env::var("AH_SELECTOR").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "fzf".to_string())
}

fn use_preview(ia: &InteractiveArgs, selector: &str) -> bool {
    let selector_bin = selector.rsplit('/').next().unwrap_or(selector);
    !ia.no_preview && PREVIEW_SELECTORS.contains(&selector_bin)
}

/// Build LTSV-format input for selector (legacy mode).
fn build_ltsv_input<F: Fn(&str) -> String>(
    key_label: &str,
    keys: &[String],
    rows: &[Vec<String>],
    field_names: &[String],
    escape: F,
) -> String {
    let mut input = String::new();
    for (i, key) in keys.iter().enumerate() {
        input.push_str(&format!("{}:{}", key_label, escape(key)));
        for (j, val) in rows[i].iter().enumerate() {
            input.push('\t');
            input.push_str(&format!("{}:{}", field_names[j], escape(val)));
        }
        input.push('\n');
    }
    input
}

pub fn run_log(args: &SearchArgs, ia: &InteractiveArgs, filter: &FilterArgs) -> Result<(), String> {
    let ltsv = args.ltsv();
    let selector = resolve_selector(ia);
    let preview = use_preview(ia, &selector);

    let display_fields = args.common.parse_fields()?.unwrap_or_else(|| {
        vec![
            Field::Agent,
            Field::Project,
            Field::ModifiedAt,
            Field::Title,
        ]
    });

    let sort_field = args.sort_field()?;
    let mut resolve_fields = vec![Field::Path, Field::Id];
    for f in &display_fields {
        if *f != Field::Path && *f != Field::Id {
            resolve_fields.push(*f);
        }
    }
    if !resolve_fields.contains(&sort_field) {
        resolve_fields.push(sort_field);
    }

    let query = filter.query.clone().unwrap_or_default();
    let result = pipeline::run_pipeline(&pipeline::PipelineParams {
        resolve_fields,
        resolve_opts: resolver::ResolveOpts::default_with_title_limit(30),
        filters: filter.to_filters(),
        since: filter.since_time()?,
        until: filter.until_time()?,
        query,
        search_mode: filter.search_mode(),
        sort_field,
        sort_order: SortOrder::Desc,
        collect_limit: filter.limit,
        running: filter.running,
        require_resume_cmd: false,
    })?;
    let sessions = result.sessions;
    let _ = result.pid_map;

    if sessions.is_empty() {
        return Err("No sessions found.".to_string());
    }

    // Extract keys (shell-quoted paths) and display rows
    let visible_fields: Vec<&Field> = display_fields
        .iter()
        .filter(|f| **f != Field::Path)
        .collect();
    let keys: Vec<String> = sessions
        .iter()
        .map(|s| shell_quote(s.fields.get(&Field::Path).map(|v| v.as_str()).unwrap_or("")))
        .collect();
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|s| {
            visible_fields
                .iter()
                .map(|f| sanitize_for_display(s.fields.get(f).map(|v| v.as_str()).unwrap_or("")))
                .collect()
        })
        .collect();

    let (input, with_nth) = if ltsv {
        let field_names: Vec<String> = visible_fields
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        let mut inp = String::new();
        for (i, key) in keys.iter().enumerate() {
            inp.push_str(&format!("path:{}", output::escape_tsv(key)));
            inp.push('\t');
            let marker = if sessions[i]
                .fields
                .get(&Field::Running)
                .is_some_and(|v| v == "true")
            {
                "\x1b[32mR\x1b[0m "
            } else {
                "  "
            };
            inp.push_str(marker);
            for (j, val) in rows[i].iter().enumerate() {
                inp.push_str(&format!("{}:{}", field_names[j], output::escape_tsv(val)));
                if j < rows[i].len() - 1 {
                    inp.push('\t');
                }
            }
            inp.push('\n');
        }
        let count = visible_fields.len();
        (inp, format!("--with-nth=2..{}", count + 2))
    } else {
        let widths = compute_column_widths(&rows);
        let colors: Vec<&str> = visible_fields
            .iter()
            .map(|f| output::field_color(f))
            .collect();
        let mut inp = String::new();
        for (i, key) in keys.iter().enumerate() {
            inp.push_str(key);
            inp.push('\t');
            let marker = if sessions[i]
                .fields
                .get(&Field::Running)
                .is_some_and(|v| v == "true")
            {
                "\x1b[32mR\x1b[0m "
            } else {
                "  "
            };
            inp.push_str(marker);
            inp.push_str(&format_columns(&rows[i], &widths, &colors));
            inp.push('\n');
        }
        (inp, "--with-nth=2..".to_string())
    };

    let mut selector_args: Vec<String> = vec![
        "--ansi".to_string(),
        "--no-sort".to_string(),
        "--delimiter=\t".to_string(),
        with_nth,
    ];

    if preview {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ah"));
        let exe_quoted = shell_quote(&exe.to_string_lossy());
        if ltsv {
            // In LTSV mode, {1} is "path:'/quoted/path'" — strip prefix via shell
            selector_args.push(format!(
                "--preview=p={{1}}; p=${{p#path:}}; {} show --color \"$p\"",
                exe_quoted
            ));
        } else {
            selector_args.push(format!("--preview={} show --color {{1}}", exe_quoted));
        }
        selector_args.push("--preview-window=right:60%:wrap".to_string());
        push_preview_search_binds(&mut selector_args, &exe_quoted, ltsv, "path", &selector);
    }

    let selected = match run_selector(&selector, &selector_args, &input)? {
        Some(s) => s,
        None => return Ok(()),
    };

    let first = selected.split('\t').next().unwrap_or("");
    let quoted = first.strip_prefix("path:").unwrap_or(first);
    let path = strip_shell_quote(quoted);
    if path.is_empty() {
        return Ok(());
    }

    println!("{}", path);
    Ok(())
}

pub fn run_project(
    args: &ListProjectsArgs,
    ia: &InteractiveArgs,
    filter: &FilterArgs,
) -> Result<(), String> {
    let ltsv = args.ltsv();
    let selector = resolve_selector(ia);
    let preview = use_preview(ia, &selector);

    let resolved = ListProjectsResolvedArgs::from_interactive(args)?;

    let records = projects::build_project_records(&resolved, filter)?;

    let display_tail: Vec<ProjectField> = resolved
        .fields
        .iter()
        .copied()
        .filter(|f| *f != ProjectField::Cwd)
        .collect();

    let keys: Vec<String> = records
        .iter()
        .map(|r| {
            r.get(&ProjectField::Cwd)
                .map(|s| s.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|r| {
            display_tail
                .iter()
                .map(|pf| sanitize_for_display(r.get(pf).map(|s| s.as_str()).unwrap_or("")))
                .collect()
        })
        .collect();

    let (input, with_nth) = if ltsv {
        let field_names: Vec<String> = display_tail.iter().map(|f| f.name().to_string()).collect();
        let inp = build_ltsv_input("cwd", &keys, &rows, &field_names, output::escape_tsv);
        let count = display_tail.len();
        let wn = if count == 0 {
            "--with-nth=1".to_string()
        } else {
            format!("--with-nth=2..{}", count + 1)
        };
        (inp, wn)
    } else {
        let widths = compute_column_widths(&rows);
        let colors: Vec<&str> = display_tail
            .iter()
            .map(|f| output::project_field_color(f))
            .collect();
        let mut inp = String::new();
        for (i, key) in keys.iter().enumerate() {
            inp.push_str(key);
            inp.push('\t');
            inp.push_str(&format_columns(&rows[i], &widths, &colors));
            inp.push('\n');
        }
        (inp, "--with-nth=2..".to_string())
    };

    let mut selector_args: Vec<String> = vec![
        "--ansi".to_string(),
        "--no-sort".to_string(),
        "--delimiter=\t".to_string(),
        with_nth,
    ];

    if preview {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ah"));
        let exe_quoted = shell_quote(&exe.to_string_lossy());
        if ltsv {
            // In LTSV mode, {1} is "cwd:'/quoted/path'" — strip prefix via shell
            selector_args.push(format!(
                "--preview=p={{1}}; p=${{p#cwd:}}; {} log -d \"$p\" -o agent,modified_at,title --color",
                exe_quoted
            ));
        } else {
            selector_args.push(format!(
                "--preview={} log -d {{1}} -o agent,modified_at,title --color",
                exe_quoted
            ));
        }
        selector_args.push("--preview-window=right:60%:wrap".to_string());
    }

    let selected = match run_selector(&selector, &selector_args, &input)? {
        Some(s) => s,
        None => return Ok(()),
    };

    let first = selected.split('\t').next().unwrap_or("");
    let cwd = first.strip_prefix("cwd:").unwrap_or(first);
    if cwd.is_empty() {
        return Ok(());
    }

    println!("{}", cwd);
    Ok(())
}

pub fn run_show(args: &ShowArgs, ia: &InteractiveArgs, filter: &FilterArgs) -> Result<(), String> {
    let selector = resolve_selector(ia);
    let preview = use_preview(ia, &selector);

    let display_fields = args.common.parse_fields()?.unwrap_or_else(|| {
        vec![
            Field::Agent,
            Field::Project,
            Field::ModifiedAt,
            Field::Title,
        ]
    });
    let mut resolve_fields = vec![Field::Path, Field::Id];
    for f in &display_fields {
        if !resolve_fields.contains(f) {
            resolve_fields.push(*f);
        }
    }

    let result = pipeline::run_pipeline(&pipeline::PipelineParams {
        resolve_fields,
        resolve_opts: resolver::ResolveOpts::default_with_title_limit(30),
        filters: filter.to_filters(),
        since: filter.since_time()?,
        until: filter.until_time()?,
        query: filter.query.clone().unwrap_or_default(),
        search_mode: filter.search_mode(),
        sort_field: Field::ModifiedAt,
        sort_order: SortOrder::Desc,
        collect_limit: filter.limit,
        running: filter.running,
        require_resume_cmd: false,
    })?;
    let sessions = result.sessions;
    let _ = result.pid_map;

    if sessions.is_empty() {
        return Err("No sessions found.".to_string());
    }

    let visible_fields: Vec<&Field> = display_fields
        .iter()
        .filter(|f| **f != Field::Path)
        .collect();
    let keys: Vec<String> = sessions
        .iter()
        .map(|s| shell_quote(s.fields.get(&Field::Path).map(|v| v.as_str()).unwrap_or("")))
        .collect();
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|s| {
            visible_fields
                .iter()
                .map(|f| sanitize_for_display(s.fields.get(f).map(|v| v.as_str()).unwrap_or("")))
                .collect()
        })
        .collect();

    let widths = compute_column_widths(&rows);
    let colors: Vec<&str> = visible_fields
        .iter()
        .map(|f| output::field_color(f))
        .collect();
    let mut input = String::new();
    for (i, key) in keys.iter().enumerate() {
        input.push_str(key);
        input.push('\t');
        let marker = if sessions[i]
            .fields
            .get(&Field::Running)
            .is_some_and(|v| v == "true")
        {
            "\x1b[32mR\x1b[0m "
        } else {
            "  "
        };
        input.push_str(marker);
        input.push_str(&format_columns(&rows[i], &widths, &colors));
        input.push('\n');
    }

    let mut selector_args: Vec<String> = vec![
        "--ansi".to_string(),
        "--no-sort".to_string(),
        "--delimiter=\t".to_string(),
        "--with-nth=2..".to_string(),
    ];

    if preview {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ah"));
        let exe_quoted = shell_quote(&exe.to_string_lossy());
        selector_args.push(format!("--preview={} show --color {{1}}", exe_quoted));
        selector_args.push("--preview-window=right:60%:wrap".to_string());
        push_preview_search_binds(&mut selector_args, &exe_quoted, false, "path", &selector);
    }

    let selected = match run_selector(&selector, &selector_args, &input)? {
        Some(s) => s,
        None => return Ok(()),
    };

    let first = selected.split('\t').next().unwrap_or("");
    let path_str = strip_shell_quote(first);
    if path_str.is_empty() {
        return Ok(());
    }

    let show_args = ShowArgs::with_session(args.head, Some(path_str), args.highlight.clone());
    crate::show::run(show_args, filter)
}

// ── Resume (interactive) ──

pub fn run_resume(
    args: &ResumeArgs,
    ia: &InteractiveArgs,
    filter: &FilterArgs,
) -> Result<(), String> {
    use crate::agents;
    use crate::resume;
    use std::fs;
    use std::time::SystemTime;

    let selector = resolve_selector(ia);
    let preview = use_preview(ia, &selector);
    let home = canonical_home();

    let display_fields = args.common.parse_fields()?.unwrap_or_else(|| {
        vec![
            Field::Agent,
            Field::Project,
            Field::ModifiedAt,
            Field::Title,
        ]
    });
    let mut resolve_fields = vec![Field::Path, Field::Id, Field::ResumeCmd];
    for f in &display_fields {
        if !resolve_fields.contains(f) {
            resolve_fields.push(*f);
        }
    }

    let result = pipeline::run_pipeline(&pipeline::PipelineParams {
        resolve_fields,
        resolve_opts: resolver::ResolveOpts::default_with_title_limit(30),
        filters: filter.to_filters(),
        since: filter.since_time()?,
        until: filter.until_time()?,
        query: filter.query.clone().unwrap_or_default(),
        search_mode: filter.search_mode(),
        sort_field: Field::ModifiedAt,
        sort_order: SortOrder::Desc,
        collect_limit: filter.limit,
        running: filter.running,
        require_resume_cmd: true,
    })?;
    let sessions = result.sessions;
    let _ = result.pid_map;

    if sessions.is_empty() {
        return Err("No resumable sessions found.".to_string());
    }

    let visible_fields: Vec<&Field> = display_fields
        .iter()
        .filter(|f| **f != Field::Path)
        .collect();
    let keys: Vec<String> = sessions
        .iter()
        .map(|s| shell_quote(s.fields.get(&Field::Path).map(|v| v.as_str()).unwrap_or("")))
        .collect();
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|s| {
            visible_fields
                .iter()
                .map(|f| sanitize_for_display(s.fields.get(f).map(|v| v.as_str()).unwrap_or("")))
                .collect()
        })
        .collect();

    let ltsv = args.ltsv;

    let (input, with_nth) = if ltsv {
        let field_names: Vec<String> = visible_fields
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        let inp = build_ltsv_input("path", &keys, &rows, &field_names, output::escape_tsv);
        let count = visible_fields.len();
        (inp, format!("--with-nth=2..{}", count + 1))
    } else {
        let widths = compute_column_widths(&rows);
        let colors: Vec<&str> = visible_fields
            .iter()
            .map(|f| output::field_color(f))
            .collect();
        let mut inp = String::new();
        for (i, key) in keys.iter().enumerate() {
            inp.push_str(key);
            inp.push('\t');
            let marker = if sessions[i]
                .fields
                .get(&Field::Running)
                .is_some_and(|v| v == "true")
            {
                "\x1b[32mR\x1b[0m "
            } else {
                "  "
            };
            inp.push_str(marker);
            inp.push_str(&format_columns(&rows[i], &widths, &colors));
            inp.push('\n');
        }
        (inp, "--with-nth=2..".to_string())
    };

    let mut selector_args: Vec<String> = vec![
        "--ansi".to_string(),
        "--no-sort".to_string(),
        "--delimiter=\t".to_string(),
        with_nth,
    ];

    if preview {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ah"));
        let exe_quoted = shell_quote(&exe.to_string_lossy());
        if ltsv {
            selector_args.push(format!(
                "--preview=p={{1}}; p=${{p#path:}}; {} show --color \"$p\"",
                exe_quoted
            ));
        } else {
            selector_args.push(format!("--preview={} show --color {{1}}", exe_quoted));
        }
        selector_args.push("--preview-window=right:60%:wrap".to_string());
        push_preview_search_binds(&mut selector_args, &exe_quoted, ltsv, "path", &selector);
    }

    let selected = match run_selector(&selector, &selector_args, &input)? {
        Some(s) => s,
        None => return Ok(()),
    };

    let first = selected.split('\t').next().unwrap_or("");
    let quoted = first.strip_prefix("path:").unwrap_or(first);
    let path_str = strip_shell_quote(quoted);
    if path_str.is_empty() {
        return Ok(());
    }

    let path = PathBuf::from(&path_str);
    let plugin = agents::find_plugin_for_path(&path);
    let mtime = fs::metadata(&path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let fields = resolver::resolve_fields(
        &path,
        plugin,
        mtime,
        &home,
        &[Field::ResumeCmd],
        &Default::default(),
    );

    let cmd = fields
        .get(&Field::ResumeCmd)
        .map(|v| v.as_str())
        .unwrap_or("");
    if cmd.is_empty() {
        return Err("No resume command available for this session.".to_string());
    }

    let full_cmd = if args.extra_args.is_empty() {
        cmd.to_string()
    } else {
        let extra = args
            .extra_args
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{} {}", cmd, extra)
    };

    if args.print {
        println!("{}", full_cmd);
        return Ok(());
    }

    resume::exec_resume(&full_cmd);
}

/// Push fzf --bind args for search-in-preview toggle (ctrl-s).
///
/// ctrl-s toggles between normal mode and search-in-preview mode.
/// State is tracked via preview-label ($FZF_PREVIEW_LABEL).
///
/// The --preview command itself checks $FZF_PREVIEW_LABEL to decide whether
/// to add --highlight, keeping {q} and {1} as live fzf placeholders.
/// This avoids nesting change-preview() inside transform() which breaks
/// fzf's parenthesis matching.
fn push_preview_search_binds(
    selector_args: &mut Vec<String>,
    exe_quoted: &str,
    ltsv: bool,
    path_prefix: &str,
    selector: &str,
) {
    let selector_bin = selector.rsplit('/').next().unwrap_or(selector);
    if selector_bin != "fzf" {
        return;
    }
    let (prefix, path_ref) = if ltsv {
        (format!("p={{1}}; p=${{p#{}:}}; ", path_prefix), "\"$p\"")
    } else {
        (String::new(), "{1}")
    };

    // Replace existing --preview with conditional version that checks $FZF_PREVIEW_LABEL
    if let Some(pos) = selector_args
        .iter()
        .position(|a| a.starts_with("--preview="))
    {
        selector_args[pos] = format!(
            "--preview={}if [ -n \"$FZF_PREVIEW_LABEL\" ] && [ -n {{q}} ]; then {} show --color --highlight={{q}} {}; else {} show --color {}; fi",
            prefix, exe_quoted, path_ref, exe_quoted, path_ref
        );
    }

    let scroll_transform = format!(
        "{}N=`{} show {} | grep -Fni -m1 -- {{q}} | cut -d: -f1`; test -n \"$N\" || N=0; echo \"change-preview-window(+$N)\"",
        prefix, exe_quoted, path_ref
    );
    // ctrl-s toggle: transform checks preview-label state and emits enter/exit actions.
    // No change-preview needed — the preview command itself handles highlight conditionally.
    selector_args.push(
        "--bind=ctrl-s:transform~if [ -n \"$FZF_PREVIEW_LABEL\" ]; then echo 'enable-search+change-preview-label()+unbind(change)+change-preview-window(+0)+refresh-preview'; else echo 'disable-search+change-preview-label([ ctrl-s: reset ])+rebind(change)+refresh-preview'; fi~".to_string()
    );
    selector_args.push(format!("--bind=change:transform({})", scroll_transform));
    selector_args.push("--bind=start:unbind(change)".to_string());
}

/// Run the selector (fzf/sk/etc) with the given args and input, return selected line.
pub fn run_memory(
    args: &MemoryArgs,
    ia: &InteractiveArgs,
    filter: &FilterArgs,
) -> Result<(), String> {
    let selector = resolve_selector(ia);
    let preview = use_preview(ia, &selector);

    // Build resolved args with Path always included for key
    let resolved = MemoryResolvedArgs::from_args_interactive(args)?;

    let records = memory::build_memory_records(&resolved, filter)?;

    // Display fields = all except Path
    let display_fields: Vec<MemoryField> = resolved
        .fields
        .iter()
        .copied()
        .filter(|f| *f != MemoryField::Path)
        .collect();

    let home = canonical_home();
    let home_str = home.to_string_lossy();
    let keys: Vec<String> = records
        .iter()
        .map(|r| {
            let p = r.get(&MemoryField::Path).map(|s| s.as_str()).unwrap_or("");
            // Expand ~ back to absolute path for preview/cat
            if let Some(rest) = p.strip_prefix('~') {
                format!("{}{}", home_str, rest)
            } else {
                p.to_string()
            }
        })
        .collect();
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|r| {
            display_fields
                .iter()
                .map(|f| sanitize_for_display(r.get(f).map(|s| s.as_str()).unwrap_or("")))
                .collect()
        })
        .collect();

    let widths = compute_column_widths(&rows);
    let colors: Vec<&str> = display_fields
        .iter()
        .map(|f| output::memory_field_color(f))
        .collect();
    let mut input = String::new();
    for (i, key) in keys.iter().enumerate() {
        input.push_str(key);
        input.push('\t');
        input.push_str(&format_columns(&rows[i], &widths, &colors));
        input.push('\n');
    }

    let mut selector_args: Vec<String> = vec![
        "--ansi".to_string(),
        "--no-sort".to_string(),
        "--delimiter=\t".to_string(),
        "--with-nth=2..".to_string(),
    ];

    if preview {
        selector_args.push("--preview=cat {1}".to_string());
        selector_args.push("--preview-window=right:60%:wrap".to_string());
    }

    let selected = match run_selector(&selector, &selector_args, &input)? {
        Some(s) => s,
        None => return Ok(()),
    };

    let path = selected.split('\t').next().unwrap_or("");
    if path.is_empty() {
        return Ok(());
    }

    println!("{}", path);
    Ok(())
}

/// Run the selector and return the selected line.
/// Returns `None` if the user cancelled (exit 0 behavior).
fn run_selector(selector: &str, args: &[String], input: &str) -> Result<Option<String>, String> {
    let mut child = Command::new(selector)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("{}: {}", selector, e))?;

    if let Some(ref mut stdin) = child.stdin {
        use std::io::Write;
        let _ = stdin.write_all(input.as_bytes());
    }
    drop(child.stdin.take());

    let result = child
        .wait_with_output()
        .map_err(|e| format!("{}: {}", selector, e))?;

    if !result.status.success() {
        // fzf: 1 = no match, 130 = cancelled (Ctrl-C). Treat as user cancellation.
        // Other non-zero codes indicate genuine errors.
        match result.status.code() {
            Some(1) | Some(130) => return Ok(None),
            Some(code) => return Err(format!("{} exited with status {}", selector, code)),
            None => return Err(format!("{} terminated by signal", selector)),
        }
    }

    let line = String::from_utf8_lossy(&result.stdout);
    let line = line.trim().to_string();
    if line.is_empty() {
        return Ok(None);
    }

    Ok(Some(line))
}

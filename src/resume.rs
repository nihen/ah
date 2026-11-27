use std::process::{self, Command};
use std::time::SystemTime;

use crate::agents;
use crate::agents::common::canonical_home;
use crate::cli::{Field, FilterArgs, ResumeArgs};
use crate::output::strip_quotes;
use crate::remote;
use crate::resolver::{self, shell_quote};
use crate::subcmd;

pub fn run(args: ResumeArgs, filter: &FilterArgs) -> Result<(), String> {
    let explicit_session = subcmd::read_session_ref(args.session.as_deref())?;
    if filter.remote.is_empty()
        && explicit_session.as_deref().is_some_and(|reference| {
            !strip_quotes(reference).contains("://")
                && crate::cloud::providers::looks_like_claude_ref(reference, &canonical_home())
        })
    {
        crate::cloud::providers::pin_claude_account(filter.agent.as_deref());
    }
    run_with_session(explicit_session, args, filter)
}

fn run_with_session(
    explicit_session: Option<String>,
    args: ResumeArgs,
    filter: &FilterArgs,
) -> Result<(), String> {
    let home = canonical_home();

    if let Some(session) = explicit_session.as_deref() {
        let unquoted = strip_quotes(session);
        if filter.remote.is_empty()
            && args.session.as_deref().is_some_and(|s| s != "-")
            && let Some(argv) =
                crate::cloud::providers::explicit_resume(unquoted, filter.agent.as_deref())
        {
            if !args.extra_args.is_empty() {
                return Err("Cloud browser resume does not accept extra arguments.".into());
            }
            let cmd = argv
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            if args.print {
                println!("{cmd}");
                return Ok(());
            }
            exec_resume(&cmd);
        }
        if let Some((remote_def, remote_ref)) = remote::parse_remote_path(unquoted) {
            if args.print {
                println!(
                    "{}",
                    remote::format_remote_resume_command(
                        remote_def,
                        remote_ref,
                        &args.extra_args,
                        filter.agent.as_deref(),
                    )
                );
                return Ok(());
            }
            remote::exec_remote_resume(remote_def, remote_ref, &args, filter.agent.as_deref());
        }
        remote::check_unknown_remote(unquoted)?;
    }

    // Only require an explicit REMOTE:REF when no session was given. With an
    // explicit local path (e.g. picked from interactive `--remote` mixed list),
    // we resume the local session even if `filter.remote` is set.
    if explicit_session.is_none() && !filter.remote.is_empty() {
        return Err(
            "Remote session resume requires an explicit REMOTE:REF session.\n\
             Example: ah resume mydev:SESSION_ID"
                .into(),
        );
    }

    let full_cmd = if let Some(session) = explicit_session.as_deref() {
        build_resume_command_for_ref(&args, filter, session, &home)?
    } else {
        // `args.session` was already consumed by `read_session_ref`; do not
        // look at it again here.
        build_resume_command_for_lookup(&args, filter, &home)?
    };
    if args.print {
        println!("{}", full_cmd);
        return Ok(());
    }
    exec_resume(&full_cmd);
}

#[cfg(test)]
fn build_resume_command(args: &ResumeArgs, filter: &FilterArgs) -> Result<String, String> {
    let home = canonical_home();
    if let Some(session) = args.session.as_deref() {
        build_resume_command_for_ref(args, filter, session, &home)
    } else {
        build_resume_command_for_lookup(args, filter, &home)
    }
}

fn build_resume_command_for_lookup(
    args: &ResumeArgs,
    filter: &FilterArgs,
    home: &std::path::Path,
) -> Result<String, String> {
    let filters = filter.to_filters();
    let path = subcmd::resolve_resumable_session(
        None,
        filter.query.as_deref(),
        &filters,
        home,
        filter.search_mode(),
        filter.since_time()?,
        filter.until_time()?,
    )?;
    build_resume_command_for_path(args, &path, home)
}

fn build_resume_command_for_ref(
    args: &ResumeArgs,
    filter: &FilterArgs,
    session: &str,
    home: &std::path::Path,
) -> Result<String, String> {
    let path = subcmd::resolve_session_ref_of(session, home, filter.agent.as_deref())?;
    build_resume_command_for_path(args, &path, home)
}

fn build_resume_command_for_path(
    args: &ResumeArgs,
    path: &std::path::Path,
    home: &std::path::Path,
) -> Result<String, String> {
    let plugin = agents::find_plugin_for_path(path);
    let mtime = plugin.session_mtime(path).unwrap_or(SystemTime::UNIX_EPOCH);
    let fields = resolver::resolve_fields(
        path,
        plugin,
        mtime,
        home,
        &[Field::ResumeCmd],
        &Default::default(),
    );

    let cmd = fields.get(&Field::ResumeCmd).map_or("", |v| v.as_str());
    if cmd.is_empty() {
        return Err("No resume command available for this session.".to_string());
    }

    if crate::cloud::providers::source_for_path(path).is_some() && !args.extra_args.is_empty() {
        return Err("Cloud resume does not accept extra arguments.".into());
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

    Ok(full_cmd)
}

pub fn exec_resume(cmd: &str) -> ! {
    exec_command(cmd)
}

#[cfg(unix)]
fn exec_command(cmd: &str) -> ! {
    use std::os::unix::process::CommandExt;
    let err = Command::new("sh").args(["-c", cmd]).exec();
    eprintln!("Failed to exec: {}", err);
    process::exit(1);
}

#[cfg(not(unix))]
fn exec_command(_cmd: &str) -> ! {
    eprintln!("resume is not supported on this platform (requires Unix)");
    process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn codex_session_copy() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let session_path = tmp
            .path()
            .join(".codex/sessions/2026/03/24/rollout-2026-03-24T20-43-12-codex-sess-001.jsonl");
        fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        fs::copy(fixture_path("codex_session.jsonl"), &session_path).unwrap();
        (tmp, session_path)
    }

    fn default_filter() -> FilterArgs {
        FilterArgs {
            all: false,
            all_remote: false,
            agent: None,
            project: None,
            dir: None,
            query: None,
            prompt_only: false,
            raw_search: false,
            limit: 0,
            running: false,
            no_archived: false,
            subagents: false,
            remote: vec![],
            since: None,
            until: None,
            color: false,
            no_color: false,
            no_pager: true,
            debug: false,
        }
    }

    #[test]
    fn build_resume_command_for_fixture_path() {
        let home = canonical_home();
        crate::config::init(&home);
        let (_tmp, session_path) = codex_session_copy();

        let args = ResumeArgs {
            common: crate::cli::CommonArgs { fields: None },
            print: true,
            session: Some(session_path.display().to_string()),
            ltsv: false,
            extra_args: Vec::new(),
        };

        let cmd = build_resume_command(&args, &default_filter()).unwrap();
        assert_eq!(
            cmd,
            "cd '/Users/test/api-server' && 'codex' 'resume' 'codex-sess-001'"
        );
    }

    #[test]
    fn build_resume_command_appends_extra_args() {
        let home = canonical_home();
        crate::config::init(&home);
        let (_tmp, session_path) = codex_session_copy();

        let args = ResumeArgs {
            common: crate::cli::CommonArgs { fields: None },
            print: true,
            session: Some(session_path.display().to_string()),
            ltsv: false,
            extra_args: vec!["--model".to_string(), "gpt-5".to_string()],
        };

        let cmd = build_resume_command(&args, &default_filter()).unwrap();
        assert_eq!(
            cmd,
            "cd '/Users/test/api-server' && 'codex' 'resume' 'codex-sess-001' '--model' 'gpt-5'"
        );
    }

    #[test]
    fn remote_flag_without_ref_returns_error() {
        let home = canonical_home();
        crate::config::init(&home);

        let args = ResumeArgs {
            common: crate::cli::CommonArgs { fields: None },
            print: false,
            session: None,
            ltsv: false,
            extra_args: Vec::new(),
        };
        let mut filter = default_filter();
        filter.remote = vec!["mydev".to_string()];

        let result = run_with_session(None, args, &filter);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Remote session resume requires an explicit REMOTE:REF")
        );
    }

    /// Runs a printed command with `ssh` replaced by a stand-in that, like
    /// OpenSSH, checks the host and hands the joined remaining arguments to
    /// a shell. The remote `ah` is a script that prints one argument per line.
    fn run_printed(cmd: &str, dir: &std::path::Path) -> String {
        let script = format!(
            "ssh() {{ [ \"$1 $2 $3\" = '-t -- mydev.example.com' ] || exit 9; shift 3; sh -c \"$*\"; }}; {cmd}"
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{cmd}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn argv_printer(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("remote ah");
        std::fs::write(&path, "#!/bin/sh\nfor a; do printf '%s\\n' \"$a\"; done\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    #[test]
    fn format_remote_resume_command_passes_arguments_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = crate::config::RemoteDef {
            name: "mydev".to_string(),
            host: "mydev.example.com".to_string(),
            ah_path: argv_printer(tmp.path()),
        };
        let cmd = remote::format_remote_resume_command(&remote, "abc123", &[], None);
        assert!(cmd.starts_with("ssh -t -- 'mydev.example.com' "), "{cmd}");
        assert_eq!(run_printed(&cmd, tmp.path()), "resume\nabc123\n");

        let cmd = remote::format_remote_resume_command(
            &remote,
            "abc123",
            &["--model".to_string(), "opus".to_string()],
            Some("codex"),
        );
        assert_eq!(
            run_printed(&cmd, tmp.path()),
            "resume\n--agent\ncodex\nabc123\n--\n--model\nopus\n"
        );
    }

    #[test]
    fn format_remote_resume_command_survives_the_remote_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = crate::config::RemoteDef {
            name: "mydev".to_string(),
            host: "mydev.example.com".to_string(),
            ah_path: argv_printer(tmp.path()),
        };
        let hostile = "a b;touch pwned1 $(touch pwned2) `touch pwned3` 'q\" -x\n|touch pwned4";
        let cmd = remote::format_remote_resume_command(&remote, hostile, &[], None);
        let out = run_printed(&cmd, tmp.path());
        for marker in ["pwned1", "pwned2", "pwned3", "pwned4"] {
            assert!(!tmp.path().join(marker).exists(), "{marker}: {cmd}");
        }
        assert_eq!(out, format!("resume\n{hostile}\n"));
    }
}

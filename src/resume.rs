use std::fs;
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
    let home = canonical_home();
    let explicit_session = subcmd::read_session_ref(args.session.as_deref());

    if let Some(session) = explicit_session.as_deref() {
        let unquoted = strip_quotes(session);
        if let Some((remote_def, remote_ref)) = remote::parse_remote_path(unquoted) {
            if args.print {
                println!(
                    "{}",
                    remote::format_remote_resume_command(remote_def, remote_ref, &args.extra_args,)
                );
                return Ok(());
            }
            remote::exec_remote_resume(remote_def, remote_ref, &args);
        }
        remote::check_unknown_remote(unquoted)?;
    }

    if !filter.remote.is_empty() {
        return Err(
            "Remote session resume requires an explicit REMOTE:REF session.\n\
             Example: ah resume mydev:SESSION_ID"
                .into(),
        );
    }

    let full_cmd = if let Some(session) = explicit_session.as_deref() {
        build_resume_command_for_ref(&args, filter, session, &home)?
    } else {
        build_resume_command(&args, filter)?
    };
    if args.print {
        println!("{}", full_cmd);
        return Ok(());
    }
    exec_resume(&full_cmd);
}

pub fn build_resume_command(args: &ResumeArgs, filter: &FilterArgs) -> Result<String, String> {
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
    _filter: &FilterArgs,
    session: &str,
    home: &std::path::Path,
) -> Result<String, String> {
    let path = subcmd::resolve_session_ref(session, home)?;
    build_resume_command_for_path(args, &path, home)
}

fn build_resume_command_for_path(
    args: &ResumeArgs,
    path: &std::path::Path,
    home: &std::path::Path,
) -> Result<String, String> {
    let plugin = agents::find_plugin_for_path(path);
    let mtime = fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let fields = resolver::resolve_fields(
        path,
        plugin,
        mtime,
        home,
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
            limit: 0,
            running: false,
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

        let result = run(args, &filter);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Remote session resume requires an explicit REMOTE:REF")
        );
    }

    #[test]
    fn format_remote_resume_command_basic() {
        let remote = crate::config::RemoteDef {
            name: "mydev".to_string(),
            host: "mydev.example.com".to_string(),
            ah_path: "/usr/local/bin/ah".to_string(),
        };
        let cmd = remote::format_remote_resume_command(&remote, "abc123", &[]);
        assert_eq!(
            cmd,
            "ssh -t -- 'mydev.example.com' '/usr/local/bin/ah' resume 'abc123'"
        );
    }

    #[test]
    fn format_remote_resume_command_with_extra_args() {
        let remote = crate::config::RemoteDef {
            name: "mydev".to_string(),
            host: "mydev.example.com".to_string(),
            ah_path: "ah".to_string(),
        };
        let cmd = remote::format_remote_resume_command(
            &remote,
            "abc123",
            &["--model".to_string(), "opus".to_string()],
        );
        assert_eq!(
            cmd,
            "ssh -t -- 'mydev.example.com' 'ah' resume 'abc123' -- '--model' 'opus'"
        );
    }
}

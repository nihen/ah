use std::fs;
use std::process::{self, Command};
use std::time::SystemTime;

use crate::agents;
use crate::agents::common::canonical_home;
use crate::cli::{Field, FilterArgs, ResumeArgs};
use crate::resolver::{self, shell_quote};
use crate::subcmd;

pub fn run(args: ResumeArgs, filter: &FilterArgs) -> Result<(), String> {
    let full_cmd = build_resume_command(&args, filter)?;
    if args.print {
        println!("{}", full_cmd);
        return Ok(());
    }
    exec_resume(&full_cmd);
}

pub fn build_resume_command(args: &ResumeArgs, filter: &FilterArgs) -> Result<String, String> {
    let home = canonical_home();

    let filters = filter.to_filters();
    let path = subcmd::resolve_resumable_session(
        args.session.as_deref(),
        filter.query.as_deref(),
        &filters,
        &home,
        filter.search_mode(),
        filter.since_time()?,
        filter.until_time()?,
    )?;

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
            agent: None,
            project: None,
            dir: None,
            query: None,
            prompt_only: false,
            limit: 0,
            running: false,
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
}

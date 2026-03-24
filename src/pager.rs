use std::sync::atomic::{AtomicBool, Ordering};

static PAGER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether a pager is currently active (stdout redirected to pager stdin).
pub fn is_active() -> bool {
    PAGER_ACTIVE.load(Ordering::Relaxed)
}

/// Set up a pager if conditions are met.
///
/// Returns `Some(Pager)` if a pager was started (caller must keep it alive),
/// or `None` if pager should not be used.
///
/// This module uses Unix-specific APIs (dup2, close) and is only available on Unix.
#[cfg(unix)]
pub fn setup(no_pager: bool) -> Option<Pager> {
    use std::os::unix::io::AsRawFd;
    use std::process::{Command, Stdio};

    if no_pager {
        return None;
    }

    // Only use pager when stdout is a TTY
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return None;
    }

    // Determine pager command: AH_PAGER → PAGER → "less"
    let pager_cmd = match std::env::var("AH_PAGER") {
        Ok(v) => v,
        Err(_) => match std::env::var("PAGER") {
            Ok(v) => v,
            Err(_) => "less".to_string(),
        },
    };

    // Empty string = disable pager (like git)
    if pager_cmd.is_empty() {
        return None;
    }

    // Ensure LESS contains F, R, X flags.
    // F: quit if output fits on one screen.
    // R: pass-through ANSI color sequences.
    // X: don't use alternate screen (prevents mouse selection conflicts in
    //    terminals like Ghostty that interpret mouse drag as scroll in altscreen).
    // Note: This is called early in main(), before any threads are spawned.
    // Build LESS flags: ensure F, R, X are present.
    // Set via Command::env to avoid unsafe set_var in a potentially multi-threaded context.
    let current = std::env::var("LESS").unwrap_or_default();
    let mut flags = current.clone();
    for c in ['F', 'R', 'X'] {
        if !current.contains(c) {
            flags.push(c);
        }
    }

    // Spawn pager process with LESS env set on the child (not the process env)
    let mut child = Command::new("sh")
        .args(["-c", &pager_cmd])
        .env("LESS", &flags)
        .stdin(Stdio::piped())
        .spawn()
        .ok()?;

    // Take the pager's stdin pipe
    let pager_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    // Redirect our stdout to the pager's stdin pipe via dup2
    // SAFETY: dup2 is a standard POSIX call. We redirect stdout to the pager's
    // stdin pipe so all subsequent println!() goes through the pager.
    unsafe {
        let pipe_fd = pager_stdin.as_raw_fd();
        if libc::dup2(pipe_fd, libc::STDOUT_FILENO) == -1 {
            drop(pager_stdin);
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        // pipe_fd is still open; it will be closed when pager_stdin is dropped
    }
    // Drop the original pipe fd (dup2 already duplicated it to stdout)
    drop(pager_stdin);

    PAGER_ACTIVE.store(true, Ordering::Relaxed);
    Some(Pager { child })
}

/// Stub for non-Unix platforms — pager is not supported.
#[cfg(not(unix))]
pub fn setup(_no_pager: bool) -> Option<Pager> {
    None
}

/// Holds the pager child process. On drop, closes stdout and waits for the pager to exit.
pub struct Pager {
    #[cfg(unix)]
    child: std::process::Child,
}

#[cfg(unix)]
impl Drop for Pager {
    fn drop(&mut self) {
        // Close our stdout (the write end of the pipe to pager stdin).
        // This signals EOF to the pager so it can finish displaying.
        unsafe {
            libc::close(libc::STDOUT_FILENO);
        }
        // Wait for pager to exit (user may press 'q' in less, etc.)
        let _ = self.child.wait();
        PAGER_ACTIVE.store(false, Ordering::Relaxed);
    }
}

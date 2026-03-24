use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

// 0 = not initialized, 1 = no color, 2 = color
static COLOR_MODE: AtomicU8 = AtomicU8::new(0);
static DEBUG_MODE: AtomicBool = AtomicBool::new(false);

/// Initialize color mode based on flags, env vars, and TTY detection.
///
/// Priority (highest first):
///   1. --no-color flag or NO_COLOR env var (standard: https://no-color.org/)
///   2. --color flag or AH_COLOR=1 env var
///   3. TTY detection (color if stdout is a terminal)
pub fn init_color(force_color: bool, no_color: bool) {
    let env_no_color = std::env::var_os("NO_COLOR").is_some();
    let env_force_color = std::env::var("AH_COLOR").is_ok_and(|v| v == "1");

    let mode = if no_color || env_no_color {
        1
    } else if force_color || env_force_color || std::io::stdout().is_terminal() {
        2
    } else {
        1
    };
    COLOR_MODE.store(mode, Ordering::Relaxed);
}

/// Check whether colored output should be used.
pub fn use_color() -> bool {
    COLOR_MODE.load(Ordering::Relaxed) == 2
}

/// Enable or disable debug mode.
pub fn init_debug(debug: bool) {
    DEBUG_MODE.store(debug, Ordering::Relaxed);
}

/// Check whether debug output is enabled.
pub fn is_debug() -> bool {
    DEBUG_MODE.load(Ordering::Relaxed)
}

// ANSI escape codes
pub const CYAN: &str = "\x1b[36m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const MAGENTA: &str = "\x1b[35m";
pub const BLUE: &str = "\x1b[34m";
pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
pub const BOLD_YELLOW: &str = "\x1b[1;33m";
pub const RESET: &str = "\x1b[0m";

/// Apply ANSI color to a string value. Returns unmodified string if color is empty.
pub fn colorize(color: &str, val: &str) -> String {
    if color.is_empty() {
        val.to_string()
    } else {
        format!("{}{}{}", color, val, RESET)
    }
}

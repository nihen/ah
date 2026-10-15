//! Cooperative cancellation for long-running scans.
//!
//! The MCP server runs tools one at a time on a single worker thread, so a
//! single process-wide flag is enough: `request()` is set by the reader
//! thread when a `notifications/cancelled` arrives for the running request,
//! scans poll `checkpoint()` in their loops and bail out, and `reset()` is
//! called before the next tool starts. CLI commands never set the flag, so
//! every checkpoint is a single relaxed atomic load there.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

static REQUESTED: AtomicBool = AtomicBool::new(false);
/// Test hook: milliseconds to sleep at every checkpoint, so small fixtures
/// still give a cancellation a window to land (`AH_MCP_TEST_CHECKPOINT_DELAY_MS`).
static DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// Ask the running scan to stop at its next checkpoint.
pub fn request() {
    REQUESTED.store(true, Ordering::Relaxed);
}

/// Clear a previous request before starting new work.
pub fn reset() {
    REQUESTED.store(false, Ordering::Relaxed);
}

/// Whether cancellation has been requested.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::Relaxed)
}

/// Poll point for scans: returns `true` when the caller should abandon its
/// work. Loops call this once per unit of work (glob, file, record).
pub fn checkpoint() -> bool {
    let delay = DELAY_MS.load(Ordering::Relaxed);
    if delay > 0 {
        std::thread::sleep(Duration::from_millis(delay));
    }
    requested()
}

/// Enable the per-checkpoint delay (MCP server only, from the test hook).
pub fn set_checkpoint_delay_ms(ms: u64) {
    DELAY_MS.store(ms, Ordering::Relaxed);
}

//! Append-only file logger for runtime errors.
//!
//! Most of bsky-vita's `eprintln!` output goes nowhere on the Vita —
//! there's no terminal attached and stderr isn't captured. This crate
//! gives us a tiny, opt-in disk log so post-mortem inspection of
//! runtime failures (worker errors, auth bounces, asset misses) is
//! possible.
//!
//! Usage:
//!
//! ```ignore
//! bsky_log::init("ux0:/data/BSKY00001/run.log");
//! bsky_log::log!("FetchSavedFeeds failed: {e}");
//! ```
//!
//! `init` truncates the file each launch (the previous run's log isn't
//! useful once the user re-launches; "did it happen this run" is the
//! useful question). Each call appends one line prefixed with the
//! seconds-since-boot timestamp. Mutex-guarded; safe across the
//! worker thread + render loop.
//!
//! Falls back gracefully (silently does nothing) if the path is
//! unwritable — we don't want a logging mishap to crash the app.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// Open (and truncate) the log at `path`. Idempotent — calling twice
/// just re-truncates. Failure is silent: subsequent `log!` calls will
/// no-op.
pub fn init(path: impl AsRef<Path>) {
    let cell = LOG.get_or_init(|| Mutex::new(None));
    let Ok(mut g) = cell.lock() else { return };
    *g = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path.as_ref())
        .ok();
    if let Some(f) = g.as_mut() {
        let _ = writeln!(f, "[0] === bsky-vita launched ===");
        let _ = f.flush();
    }
}

/// Append one line to the log. No-op if `init` hasn't run or the file
/// couldn't be opened. Each line is prefixed with seconds-since-epoch
/// for rough chronology.
pub fn log_line(line: &str) {
    let cell = LOG.get_or_init(|| Mutex::new(None));
    let Ok(mut g) = cell.lock() else { return };
    let Some(f) = g.as_mut() else { return };
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "[{secs}] {line}");
    let _ = f.flush();
}

/// Convenience macro: `bsky_log::log!("FetchFoo failed: {e}")`.
/// Mirrors `eprintln!` but appends to the disk log instead of stderr.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        $crate::log_line(&format!($($arg)*));
    }};
}

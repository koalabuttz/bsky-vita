//! Crash-resilient JSON persistence shared by the session stores.
//!
//! Both session stores write with: serialize → write `<path>.json.tmp` →
//! `remove(main)` → `rename(tmp → main)`. On Vita's FAT/exFAT the remove/rename
//! can fail or be interrupted, leaving a *complete* `.tmp` that never replaced a
//! now-stale `main`. Because atproto refresh tokens are one-time-use, a stale
//! `main` is a dead session — a single missed install permanently locks the user
//! out.
//!
//! [`load_json_recovering`] fixes that: a complete `.tmp` is always the freshest
//! write (writes go tmp→main), so it's preferred and promoted. This also makes
//! the write resilient to Vita's flaky rename-over-existing entirely — a
//! never-installed `.tmp` self-corrects on the next load. [`delete_json`] removes
//! both files so a logout/auth-failure reset can't be undone by recovery.
//!
//! The write side is intentionally left in each store unchanged: it already
//! produces a complete `.tmp` before installing, which is all recovery needs.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::DeserializeOwned;

/// Sidecar tmp path: `…/session.json` → `…/session.json.tmp`.
fn tmp_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

/// Load the freshest valid value for `path`, recovering from an orphaned `.tmp`
/// left by an interrupted/failed write. A complete `.tmp` wins over `main` and is
/// promoted; a torn (unparseable) `.tmp` is discarded; otherwise `main` is loaded.
/// Returns `None` if neither holds a valid value.
pub fn load_json_recovering<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let tmp = tmp_path(path);
    if let Ok(bytes) = std::fs::read(&tmp) {
        match serde_json::from_slice::<T>(&bytes) {
            Ok(value) => {
                // Freshest write that never installed → promote over stale main.
                // Best-effort: the value is already in hand; a failed promote just
                // means the next load re-recovers from the same `.tmp`.
                let _ = std::fs::remove_file(path);
                let _ = std::fs::rename(&tmp, path);
                bsky_log::log!("session: recovered freshest session from .tmp");
                return Some(value);
            }
            Err(_) => {
                // Torn/partial `.tmp` — never trust it; drop it and use main.
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Delete the persisted JSON at `path` **and** its `.tmp` sidecar, so a logout
/// can't be silently undone by [`load_json_recovering`] resurrecting the `.tmp`.
/// Both removals are attempted; returns the first error, if any.
pub fn delete_json(path: &Path) -> std::io::Result<()> {
    let tmp = tmp_path(path);
    let r_main = if path.exists() { std::fs::remove_file(path) } else { Ok(()) };
    let r_tmp = if tmp.exists() { std::fs::remove_file(&tmp) } else { Ok(()) };
    r_main.and(r_tmp)
}

/// Process-wide session-write gate, holding the current "session generation".
/// Every session store captures the generation at construction (via
/// [`current_session_generation`]); a logout bumps it
/// ([`invalidate_and_delete_sessions`]). A store built before the bump is
/// stale, so [`with_session_write_gate`] refuses its writes — this is what
/// stops a worker that is mid-token-refresh during logout from re-persisting
/// (resurrecting) a session we just deleted. The single `Mutex` also serializes
/// each write against the delete, closing the check-then-write race.
static SESSION_GATE: Mutex<u64> = Mutex::new(0);

fn gate() -> std::sync::MutexGuard<'static, u64> {
    SESSION_GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The current session generation. A session store records this when built and
/// later passes it to [`with_session_write_gate`] on every write.
pub fn current_session_generation() -> u64 {
    *gate()
}

/// Run `write` only if `generation` is still current — i.e. no logout happened
/// since the store that owns it was built. The gate is held for the duration of
/// `write`, serializing it against [`invalidate_and_delete_sessions`] so an
/// in-flight token-refresh write cannot interleave with the logout delete. A
/// stale generation returns `Ok(())` with the write skipped.
pub fn with_session_write_gate<E>(
    generation: u64,
    write: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    let guard = gate();
    if *guard != generation {
        return Ok(());
    }
    write()
}

/// Logout / auth-failure teardown: bump the session generation (invalidating
/// every store built before now) and delete `paths` (each with its `.tmp`
/// sidecar) — all under the gate, atomically w.r.t. any in-flight write. Call
/// this BEFORE dropping the worker; a worker mid-refresh then either already
/// wrote (and is deleted here) or sees the bumped generation and skips.
pub fn invalidate_and_delete_sessions(paths: &[&Path]) {
    let mut guard = gate();
    *guard = guard.wrapping_add(1);
    for p in paths {
        let _ = delete_json(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Demo {
        v: u32,
    }

    fn write(path: &Path, v: u32) {
        std::fs::write(path, serde_json::to_vec(&Demo { v }).unwrap()).unwrap();
    }

    #[test]
    fn loads_main_when_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        write(&p, 7);
        assert_eq!(load_json_recovering::<Demo>(&p), Some(Demo { v: 7 }));
        assert!(!tmp_path(&p).exists());
    }

    #[test]
    fn recovers_and_promotes_valid_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        write(&p, 1); // stale main (e.g. consumed refresh token)
        write(&tmp_path(&p), 2); // freshest write that never installed
        assert_eq!(
            load_json_recovering::<Demo>(&p),
            Some(Demo { v: 2 }),
            "prefers the freshest .tmp"
        );
        assert!(!tmp_path(&p).exists(), ".tmp promoted away");
        assert_eq!(
            load_json_recovering::<Demo>(&p),
            Some(Demo { v: 2 }),
            "promoted into main"
        );
    }

    #[test]
    fn rejects_torn_tmp_falls_back_to_main() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        write(&p, 5);
        std::fs::write(tmp_path(&p), b"{ not valid json").unwrap();
        assert_eq!(
            load_json_recovering::<Demo>(&p),
            Some(Demo { v: 5 }),
            "ignores torn .tmp, uses main"
        );
        assert!(!tmp_path(&p).exists(), "torn .tmp discarded");
    }

    #[test]
    fn recovers_tmp_when_main_missing() {
        // Crash between remove(main) and rename(tmp -> main).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        write(&tmp_path(&p), 9);
        assert_eq!(load_json_recovering::<Demo>(&p), Some(Demo { v: 9 }));
        assert!(p.exists() && !tmp_path(&p).exists());
    }

    #[test]
    fn delete_clears_main_and_tmp_no_resurrection() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        write(&p, 1);
        write(&tmp_path(&p), 2);
        delete_json(&p).unwrap();
        assert!(!p.exists() && !tmp_path(&p).exists());
        assert_eq!(load_json_recovering::<Demo>(&p), None, "no resurrection");
    }

    #[test]
    fn session_write_gate_blocks_stale_writes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let g0 = current_session_generation();
        // Current generation → write runs.
        with_session_write_gate::<()>(g0, || {
            write(&p, 1);
            Ok(())
        })
        .unwrap();
        assert!(p.exists());
        // Logout: bump generation + delete (atomic w.r.t. writes).
        invalidate_and_delete_sessions(&[p.as_path()]);
        assert!(!p.exists());
        let g1 = current_session_generation();
        assert_ne!(g0, g1, "generation bumped on invalidate");
        // A store built before logout (g0) is now stale → its write is skipped.
        with_session_write_gate::<()>(g0, || {
            write(&p, 2);
            Ok(())
        })
        .unwrap();
        assert!(!p.exists(), "stale-generation write must not resurrect the file");
        // A freshly-built store captures g1 → writes allowed again.
        with_session_write_gate::<()>(g1, || {
            write(&p, 3);
            Ok(())
        })
        .unwrap();
        assert!(p.exists());
    }
}

//! File-backed [`atrium_oauth::store::session::SessionStore`] persisting the
//! completed OAuth session (DPoP key + token set) to
//! `ux0:data/BSKY00001/auth/oauth-session.json`.
//!
//! v1 holds a single session keyed by DID. The store does NOT enumerate
//! all sessions (multi-account is a v1.x concern) — `get_persisted_did()`
//! lets the resume path discover the one DID we have on disk so it can
//! call `OAuthClient::restore(did)`.
//!
//! Atomic writes mirror [`bsky_auth::FileSessionStore`]'s pattern: write
//! to `<path>.tmp`, then `remove(path)` (Vita's rename-over-existing is
//! not reliable on all firmware paths), then `rename(tmp -> path)`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

use atrium_api::types::string::Did;
use atrium_common::store::Store;
use atrium_oauth::store::session::{Session, SessionStore};
use serde::{Deserialize, Serialize};

/// On-disk envelope. Wraps the session with its DID so resume can find
/// "the one persisted account" without an enumeration API.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedEnvelope {
    did: Did,
    session: Session,
}

pub struct FileOAuthSessionStore {
    path: PathBuf,
    cached: Mutex<Option<PersistedEnvelope>>,
    /// Session generation captured at construction; a logout bumps the global
    /// generation (see [`crate::atomic_json::invalidate_and_delete_sessions`]),
    /// making this store stale so its writes are refused — preventing a worker
    /// mid-refresh from resurrecting a just-deleted session.
    generation: u64,
}

#[derive(Debug)]
pub enum OAuthSessionStoreError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Poisoned,
}

impl std::fmt::Display for OAuthSessionStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthSessionStoreError::Io(e) => write!(f, "io: {e}"),
            OAuthSessionStoreError::Json(e) => write!(f, "json: {e}"),
            OAuthSessionStoreError::Poisoned => write!(f, "oauth session mutex poisoned"),
        }
    }
}

impl std::error::Error for OAuthSessionStoreError {}

impl From<std::io::Error> for OAuthSessionStoreError {
    fn from(e: std::io::Error) -> Self {
        OAuthSessionStoreError::Io(e)
    }
}

impl From<serde_json::Error> for OAuthSessionStoreError {
    fn from(e: serde_json::Error) -> Self {
        OAuthSessionStoreError::Json(e)
    }
}

impl FileOAuthSessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let cached = Mutex::new(load_from_disk(&path));
        let generation = crate::atomic_json::current_session_generation();
        Self { path, cached, generation }
    }

    /// True iff a persisted session is currently loaded from disk.
    pub fn has_session(&self) -> bool {
        self.cached.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// The DID of the currently persisted session, if any. Used by the
    /// resume path to choose which `restore(did)` to call.
    pub fn get_persisted_did(&self) -> Option<Did> {
        self.cached
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|env| env.did.clone()))
    }

    fn write_to_disk(&self, env: &PersistedEnvelope) -> Result<(), OAuthSessionStoreError> {
        // Gated on the session generation: a logout that bumped it makes this
        // store stale and the write is skipped (the gate also serializes the
        // write against the logout delete, so neither can interleave).
        crate::atomic_json::with_session_write_gate(self.generation, || {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = serde_json::to_vec_pretty(env)?;
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, &bytes)?;
            if self.path.exists() {
                std::fs::remove_file(&self.path)?;
            }
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })
    }

    fn delete_from_disk(&self) -> Result<(), OAuthSessionStoreError> {
        // Removes both the main file and its `.tmp` sidecar (so a deleted
        // session can't be resurrected by `.tmp` recovery).
        crate::atomic_json::delete_json(&self.path)?;
        Ok(())
    }
}

fn load_from_disk(path: &PathBuf) -> Option<PersistedEnvelope> {
    // Recovers the freshest tokens from an orphaned `.tmp` left by an
    // interrupted write — the fix for the one-time-use refresh-token lockout.
    crate::atomic_json::load_json_recovering(path)
}

impl Store<Did, Session> for FileOAuthSessionStore {
    type Error = OAuthSessionStoreError;

    fn get(
        &self,
        key: &Did,
    ) -> impl Future<Output = Result<Option<Session>, Self::Error>> + Send {
        let snapshot = self
            .cached
            .lock()
            .map(|g| {
                g.as_ref().and_then(|env| {
                    if &env.did == key {
                        Some(env.session.clone())
                    } else {
                        None
                    }
                })
            })
            .map_err(|_| OAuthSessionStoreError::Poisoned);
        async move { snapshot }
    }

    fn set(
        &self,
        key: Did,
        value: Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let env = PersistedEnvelope { did: key, session: value };
        let result: Result<(), OAuthSessionStoreError> = (|| {
            *self
                .cached
                .lock()
                .map_err(|_| OAuthSessionStoreError::Poisoned)? = Some(env.clone());
            self.write_to_disk(&env)
        })();
        async move { result }
    }

    fn del(&self, key: &Did) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let key = key.clone();
        let result: Result<(), OAuthSessionStoreError> = (|| {
            let mut g = self
                .cached
                .lock()
                .map_err(|_| OAuthSessionStoreError::Poisoned)?;
            if g.as_ref().map(|env| env.did == key).unwrap_or(false) {
                *g = None;
                drop(g);
                self.delete_from_disk()?;
            }
            Ok(())
        })();
        async move { result }
    }

    fn clear(&self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result: Result<(), OAuthSessionStoreError> = (|| {
            *self
                .cached
                .lock()
                .map_err(|_| OAuthSessionStoreError::Poisoned)? = None;
            self.delete_from_disk()
        })();
        async move { result }
    }
}

impl SessionStore for FileOAuthSessionStore {}

//! File-backed [`AtpSessionStore`] persisting to `ux0:data/BSKY00001/auth/session.json`.
//!
//! Atomic writes: write to `<path>.tmp`, then `rename` to `<path>`. On
//! Vita's filesystem, `rename` over an existing file may not be atomic on all
//! firmware paths; we explicitly remove the destination first if it exists,
//! then rename. For Phase 1 this is sufficient.
//!
//! ## Trait stack
//!
//! - [`Store<(), AtpSession>`](atrium_common::store::Store) — get/set/del/clear.
//!   Atrium's agent calls these to persist sessions across login + refresh.
//! - [`AuthorizationProvider`](atrium_api::agent::AuthorizationProvider) — the
//!   agent's XRPC layer asks the store for the current `Bearer` token (access
//!   for normal calls, refresh for the refresh-session call itself).
//! - [`AtpSessionStore`](atrium_api::agent::atp_agent::store::AtpSessionStore) —
//!   marker that combines the two.

use atrium_api::agent::AuthorizationProvider;
use atrium_api::agent::atp_agent::AtpSession;
use atrium_api::agent::atp_agent::store::AtpSessionStore;
use atrium_common::store::Store;
use atrium_xrpc::types::AuthorizationToken;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

/// File-backed session store. The session is held in a `Mutex<Option<_>>` for
/// fast read access; writes also flush to disk. Loaded lazily on construction.
pub struct FileSessionStore {
    path: PathBuf,
    cached: Mutex<Option<AtpSession>>,
    /// Session generation captured at construction. A logout bumps the global
    /// generation (see [`bsky_oauth::atomic_json::invalidate_and_delete_sessions`]),
    /// making this store stale so its writes are refused — preventing a worker
    /// mid-refresh from resurrecting a just-deleted session.
    generation: u64,
}

#[derive(Debug)]
pub enum SessionStoreError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Poisoned,
}

impl std::fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionStoreError::Io(e) => write!(f, "io: {e}"),
            SessionStoreError::Json(e) => write!(f, "json: {e}"),
            SessionStoreError::Poisoned => write!(f, "session mutex poisoned"),
        }
    }
}

impl std::error::Error for SessionStoreError {}

impl From<std::io::Error> for SessionStoreError {
    fn from(e: std::io::Error) -> Self {
        SessionStoreError::Io(e)
    }
}

impl From<serde_json::Error> for SessionStoreError {
    fn from(e: serde_json::Error) -> Self {
        SessionStoreError::Json(e)
    }
}

impl FileSessionStore {
    /// Create a new store rooted at `path`. Loads any existing session into
    /// the in-memory cache.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let cached = Mutex::new(load_from_disk(&path));
        let generation = bsky_oauth::atomic_json::current_session_generation();
        Self { path, cached, generation }
    }

    /// Has the store been loaded with an existing session? (Cheap read.)
    pub fn has_session(&self) -> bool {
        self.cached
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    fn write_to_disk(&self, session: &AtpSession) -> Result<(), SessionStoreError> {
        // Gated on the session generation: a logout that bumped it makes this
        // store stale and the write is skipped (the gate also serializes the
        // write against the logout delete, so neither can interleave).
        bsky_oauth::atomic_json::with_session_write_gate(self.generation, || {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = serde_json::to_vec_pretty(session)?;
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, &bytes)?;
            // Vita's filesystem may not allow atomic rename over an existing
            // file; be conservative and remove the target first if it exists.
            if self.path.exists() {
                std::fs::remove_file(&self.path)?;
            }
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })
    }

    fn delete_from_disk(&self) -> Result<(), SessionStoreError> {
        // Removes both the main file and its `.tmp` sidecar (shared helper) so a
        // logged-out session can't be resurrected by `.tmp` recovery.
        bsky_oauth::atomic_json::delete_json(&self.path)?;
        Ok(())
    }
}

fn load_from_disk(path: &PathBuf) -> Option<AtpSession> {
    // Recovers the freshest session from an orphaned `.tmp` left by an
    // interrupted write (atproto refresh JWTs rotate too — same lockout risk).
    bsky_oauth::atomic_json::load_json_recovering(path)
}

impl Store<(), AtpSession> for FileSessionStore {
    type Error = SessionStoreError;

    fn get(
        &self,
        _key: &(),
    ) -> impl Future<Output = Result<Option<AtpSession>, Self::Error>> + Send {
        let snapshot = self
            .cached
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| SessionStoreError::Poisoned);
        async move { snapshot }
    }

    fn set(
        &self,
        _key: (),
        value: AtpSession,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        // Update the cache synchronously, then attempt the disk write.
        // Disk failures bubble up — we'd rather see the error than silently
        // diverge cache from disk.
        let result: Result<(), SessionStoreError> = (|| {
            *self
                .cached
                .lock()
                .map_err(|_| SessionStoreError::Poisoned)? = Some(value.clone());
            self.write_to_disk(&value)
        })();
        async move { result }
    }

    fn del(&self, _key: &()) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result: Result<(), SessionStoreError> = (|| {
            *self
                .cached
                .lock()
                .map_err(|_| SessionStoreError::Poisoned)? = None;
            self.delete_from_disk()
        })();
        async move { result }
    }

    fn clear(&self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result: Result<(), SessionStoreError> = (|| {
            *self
                .cached
                .lock()
                .map_err(|_| SessionStoreError::Poisoned)? = None;
            self.delete_from_disk()
        })();
        async move { result }
    }
}

impl AuthorizationProvider for FileSessionStore {
    fn authorization_token(
        &self,
        is_refresh: bool,
    ) -> impl Future<Output = Option<AuthorizationToken>> + Send {
        let token = self
            .cached
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|s| {
                if is_refresh {
                    s.data.refresh_jwt.clone()
                } else {
                    s.data.access_jwt.clone()
                }
            }))
            .map(AuthorizationToken::Bearer);
        async move { token }
    }
}

impl AtpSessionStore for FileSessionStore {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_session() -> AtpSession {
        use atrium_api::com::atproto::server::create_session::OutputData;
        OutputData {
            access_jwt: "access-jwt-1".into(),
            active: None,
            did: "did:plc:test123".parse().expect("valid did"),
            did_doc: None,
            email: None,
            email_auth_factor: None,
            email_confirmed: None,
            handle: "alice.example.com".parse().expect("valid handle"),
            refresh_jwt: "refresh-jwt-1".into(),
            status: None,
        }
        .into()
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("session.json");

        // Write
        let store = FileSessionStore::new(&path);
        assert!(!store.has_session());
        let session = dummy_session();
        let result =
            futures::executor::block_on(<FileSessionStore as Store<(), AtpSession>>::set(
                &store, (), session.clone(),
            ));
        assert!(result.is_ok(), "set: {result:?}");
        assert!(store.has_session());
        assert!(path.exists());

        // Read back from a fresh instance
        let store2 = FileSessionStore::new(&path);
        assert!(store2.has_session());
        let got = futures::executor::block_on(
            <FileSessionStore as Store<(), AtpSession>>::get(&store2, &()),
        )
        .expect("get ok")
        .expect("session present");
        assert_eq!(got.data.access_jwt, "access-jwt-1");
        assert_eq!(got.data.refresh_jwt, "refresh-jwt-1");
    }

    #[test]
    fn authorization_token_returns_correct_jwt() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("session.json");
        let store = FileSessionStore::new(&path);
        let session = dummy_session();
        futures::executor::block_on(<FileSessionStore as Store<(), AtpSession>>::set(
            &store, (), session,
        ))
        .expect("set");

        let access = futures::executor::block_on(store.authorization_token(false));
        let refresh = futures::executor::block_on(store.authorization_token(true));
        assert!(matches!(
            access,
            Some(AuthorizationToken::Bearer(s)) if s == "access-jwt-1"
        ));
        assert!(matches!(
            refresh,
            Some(AuthorizationToken::Bearer(s)) if s == "refresh-jwt-1"
        ));
    }

    #[test]
    fn del_removes_file_and_cache() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("session.json");
        let store = FileSessionStore::new(&path);
        futures::executor::block_on(<FileSessionStore as Store<(), AtpSession>>::set(
            &store, (), dummy_session(),
        ))
        .expect("set");
        assert!(path.exists());
        futures::executor::block_on(<FileSessionStore as Store<(), AtpSession>>::del(
            &store, &(),
        ))
        .expect("del");
        assert!(!path.exists());
        assert!(!store.has_session());
    }
}

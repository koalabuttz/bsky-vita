//! `AuthClient` — the bundle of resources the rest of the app holds onto
//! after a successful login or resume.
//!
//! Two constructors:
//! - [`login_with_password`] — starts a fresh session against a (possibly
//!   custom) PDS using a Bluesky app password.
//! - [`try_resume_existing_session`] — if `session.json` exists on disk,
//!   re-resolves the user's PDS and calls `resume_session` to refresh the
//!   token pair.
//!
//! Both end with the same shape: a configured [`atrium_api::agent::atp_agent::AtpAgent`]
//! plus the resolved DID + PDS URL, ready for any subsequent
//! `agent.api.app.bsky.*` call.

use std::sync::Arc;

use atrium_api::agent::atp_agent::AtpAgent;
use bsky_net::VitaHttpClient;
use futures::executor::block_on;

use crate::resolver::{resolve_pds, ResolvedIdentity};
use crate::store::FileSessionStore;
use crate::xrpc::PdsClient;
use crate::{AuthError, SESSION_PATH};

/// Concrete agent + identity bundle that flows from LoginScreen into
/// ProfileScreen (and any later authenticated screen).
pub struct AuthClient {
    pub agent: AtpAgent<FileSessionStore, PdsClient>,
    pub resolved: ResolvedIdentity,
}

/// Fresh login: resolve the handle's current PDS, build an agent against
/// that PDS, call `createSession`. Persists session to disk via the
/// agent's `FileSessionStore`.
///
/// Synchronous — drives atrium's async traits with `block_on` at the
/// boundary. Suitable for calling from a screen's `after_present` (where
/// the user has already seen an "Authenticating…" frame).
pub fn login_with_password(handle: &str, app_password: &str) -> Result<AuthClient, AuthError> {
    let http_client = Arc::new(VitaHttpClient::new());

    // Step 1: handle → DID → PDS URL.
    let resolved = block_on(resolve_pds(Arc::clone(&http_client), handle))?;

    // Step 2: build the agent pointed at that PDS, with persistent storage.
    let pds_client = PdsClient::new(http_client, &resolved.pds);
    let store = FileSessionStore::new(SESSION_PATH);
    let agent = AtpAgent::new(pds_client, store);

    // Step 3: createSession (the agent persists the result via the store).
    block_on(agent.login(handle, app_password))
        .map_err(|e| AuthError::Login(format!("{e}")))?;

    Ok(AuthClient { agent, resolved })
}

/// If `session.json` exists, build an agent and call `resume_session`
/// (which validates the access JWT and auto-refreshes if needed). Returns
/// `Ok(None)` if no session is on disk OR the resume failed (e.g. tokens
/// expired beyond their 14-day refresh window) — callers should fall
/// back to fresh login in those cases.
///
/// We re-resolve the PDS at startup rather than caching it alongside the
/// session: one extra public.api.bsky.app round-trip on launch is small,
/// and it avoids stale-PDS bugs if a user's account migrates between
/// hosts.
pub fn try_resume_existing_session() -> Result<Option<AuthClient>, AuthError> {
    let store = FileSessionStore::new(SESSION_PATH);
    if !store.has_session() {
        return Ok(None);
    }

    // Pull the session out so we can re-resolve from its handle, then move
    // the store into the agent for everything that follows.
    use atrium_common::store::Store;
    let session = block_on(<FileSessionStore as Store<
        (),
        atrium_api::agent::atp_agent::AtpSession,
    >>::get(&store, &()))?;
    let session = match session {
        Some(s) => s,
        None => return Ok(None),
    };

    let http_client = Arc::new(VitaHttpClient::new());
    let resolved = block_on(resolve_pds(
        Arc::clone(&http_client),
        session.data.handle.as_str(),
    ))?;
    let pds_client = PdsClient::new(http_client, &resolved.pds);
    let agent = AtpAgent::new(pds_client, store);

    match block_on(agent.resume_session(session)) {
        Ok(()) => Ok(Some(AuthClient { agent, resolved })),
        Err(_) => {
            // Refresh failed (or getSession bounced). Treat as no session;
            // the caller will route to LoginScreen. We deliberately don't
            // delete session.json here — leave that to an explicit logout.
            Ok(None)
        }
    }
}

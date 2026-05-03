//! bsky-vita: PS Vita homebrew Bluesky client.
//!
//! Phase 1: headless authenticated session against any PDS.
//!
//! Reads `ux0:data/BSKY00001/credentials.toml`, resolves the handle to its
//! current PDS (works for bsky.social *and* custom PDSes like yapfest.club),
//! creates a session via app password, persists tokens to
//! `ux0:data/BSKY00001/auth/session.json`, and fetches the user's own profile
//! via `app.bsky.actor.getProfile`. The full report is written to
//! `ux0:data/BSKY00001/spike.log` for retrieval over vitacompanion's FTP.
//!
//! Screen stays black; render skeleton + login UI are Phase 2.

use std::fmt::Write as _;
use std::sync::Arc;

use atrium_api::app::bsky::actor::get_profile;
use bsky_auth::{
    AuthError, FileSessionStore, PdsClient, ResolvedIdentity, CREDENTIALS_PATH, SESSION_PATH,
    load_credentials, resolve_pds,
};
use bsky_models::AtpSession;
use bsky_net::VitaHttpClient;
use futures::executor::block_on;

const LOG_PATH: &str = "ux0:data/BSKY00001/spike.log";

fn main() {
    let report = run_phase1();
    println!("{report}");

    let _ = std::fs::write(LOG_PATH, &report);

    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn run_phase1() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== bsky-vita: phase 1 ===");

    match block_on(run_inner(&mut out)) {
        Ok(()) => {}
        Err(e) => {
            let _ = writeln!(out, "ERROR: {e}");
        }
    }

    let _ = writeln!(out, "=== phase 1 done ===");
    out
}

/// Async body. We use exactly one `block_on` call site — at the boundary in
/// `run_phase1` — to keep tokio out of the binary. Atrium's `tokio::sync`
/// primitives are runtime-agnostic and tolerate `futures::executor::block_on`.
async fn run_inner(out: &mut String) -> Result<(), AuthError> {
    let creds = load_credentials(CREDENTIALS_PATH)?;
    let _ = writeln!(out, "handle (from credentials.toml): {}", creds.handle);

    // Single shared HTTP client — used by the resolver AND the agent's PDS
    // client below. Arc-wrapped so atrium-identity can hold its own ref.
    let http_client = Arc::new(VitaHttpClient::new());

    // Step 1: resolve handle → DID → DID document → PDS URL.
    let ResolvedIdentity { did, pds } =
        resolve_pds(Arc::clone(&http_client), &creds.handle).await?;
    let _ = writeln!(out, "resolved did: {did}");
    let _ = writeln!(out, "resolved pds: {pds}");

    // Step 2: build the agent (PDS-bound XRPC client + persistent session store).
    let pds_client = PdsClient::new(http_client, &pds);
    let store = FileSessionStore::new(SESSION_PATH);
    let already_have_session = store.has_session();
    let _ = writeln!(out, "session.json present: {already_have_session}");

    let agent = atrium_api::agent::atp_agent::AtpAgent::new(pds_client, store);

    // Step 3: log in (or resume an existing session if one is on disk).
    let session = if already_have_session {
        // The store already loaded the cached session at construction time.
        // resume_session does a getSession round-trip (which auto-refreshes
        // the access JWT if it's expired) and updates the store accordingly.
        match agent.get_session().await {
            Some(existing) => match agent.resume_session(existing.clone()).await {
                Ok(()) => {
                    let _ = writeln!(out, "resumed existing session");
                    agent.get_session().await.unwrap_or(existing)
                }
                Err(e) => {
                    let _ = writeln!(
                        out,
                        "resume failed ({e}), falling back to fresh login"
                    );
                    fresh_login(&agent, &creds.handle, &creds.app_password).await?
                }
            },
            None => fresh_login(&agent, &creds.handle, &creds.app_password).await?,
        }
    } else {
        fresh_login(&agent, &creds.handle, &creds.app_password).await?
    };

    let _ = writeln!(out, "did: {}", session.data.did.as_str());
    let _ = writeln!(out, "handle: {}", session.data.handle.as_str());
    let _ = writeln!(out, "access_jwt len: {}", session.data.access_jwt.len());
    let _ = writeln!(out, "refresh_jwt len: {}", session.data.refresh_jwt.len());

    // Step 4: getProfile against the user's own DID, via the (now authenticated)
    // agent's PDS — which proxies to the AppView for app.bsky.* reads.
    let profile = agent
        .api
        .app
        .bsky
        .actor
        .get_profile(
            get_profile::ParametersData {
                actor: session.data.did.clone().into(),
            }
            .into(),
        )
        .await
        .map_err(|e| AuthError::Other(format!("getProfile failed: {e}")))?;

    match serde_json::to_string_pretty(&*profile) {
        Ok(json) => {
            let head = &json[..json.len().min(800)];
            let _ = writeln!(out, "getProfile (first 800 bytes):\n{head}");
        }
        Err(e) => {
            let _ = writeln!(out, "getProfile JSON serialize failed: {e}");
        }
    }

    Ok(())
}

async fn fresh_login(
    agent: &atrium_api::agent::atp_agent::AtpAgent<FileSessionStore, PdsClient>,
    handle: &str,
    password: &str,
) -> Result<AtpSession, AuthError> {
    agent
        .login(handle, password)
        .await
        .map_err(|e| AuthError::Login(format!("{e}")))
}

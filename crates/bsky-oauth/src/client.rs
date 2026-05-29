//! [`VitaOAuthClient`] — wraps [`atrium_oauth::OAuthClient`] with our concrete
//! transport, resolvers, and persistence.
//!
//! Lifecycle (one OAuth login):
//!
//! 1. `LoginScreen` builds a `VitaOAuthClient` when the user taps "Sign in
//!    with Bluesky".
//! 2. `start_flow(handle, Transport::Broker)` → PAR call → returns the
//!    authorize URL (for QR display) + the `state` nonce (for broker
//!    polling). The atrium-oauth internal state-store captures the
//!    in-flight PKCE+DPoP material keyed by `state`.
//! 3. The Vita renders the QR; user scans + consents on phone; the broker
//!    receives `(code, state, iss)`; the Vita polls `/pop?state=…` and
//!    picks them up.
//! 4. `complete_flow(state, code, iss)` → atrium-oauth exchanges the code
//!    for tokens via the token endpoint (DPoP-signed), persists the
//!    `Session { dpop_key, token_set }` to our [`FileOAuthSessionStore`],
//!    and returns an [`OAuthLoginResult`] holding the live
//!    [`crate::OAuthAgent`].
//! 5. The caller wraps the result into `bsky_auth::AuthClient` via the
//!    `AuthAgent::OAuth(...)` variant and emits `AuthComplete`.
//!
//! Subsequent app launches go through `try_resume_existing_oauth_session()`
//! which looks up the persisted DID and calls `OAuthClient::restore(did)`
//! to rebuild a live session (transparently refreshing the access token
//! if it's expired but the refresh token is still valid).

use std::future::Future;
use std::sync::{Arc, Mutex};

use atrium_api::agent::Agent;
use atrium_api::did_doc::DidDocument;
use atrium_api::types::string::Did;
use atrium_common::resolver::Resolver;
use atrium_common::store::Store;
use atrium_identity::did::{
    CommonDidResolver, CommonDidResolverConfig, DEFAULT_PLC_DIRECTORY_URL,
};
use atrium_identity::handle::{AppViewHandleResolver, AppViewHandleResolverConfig};
use atrium_identity::identity_resolver::ResolvedIdentity;
use atrium_oauth::store::state::{InternalStateData, StateStore};
use atrium_oauth::{
    AtprotoClientMetadata, AuthMethod, AuthorizeOptions, CallbackParams, GrantType,
    KnownScope, OAuthClient, OAuthClientConfig, OAuthResolverConfig, Scope,
};
use bsky_net::VitaHttpClient;
use futures::executor::block_on;

use crate::session_store::{FileOAuthSessionStore, OAuthSessionStoreError};
use crate::{
    OAuthAgent, CLIENT_METADATA_URL, HANDLE_RESOLVER_URL, OAUTH_SESSION_PATH,
    REDIRECT_URI_BROKER, REDIRECT_URI_QR,
};

// Concrete generic params for `OAuthClient`. T = VitaHttpClient (not Arc) —
// atrium-oauth `Arc`s its http_client internally; passing Arc<...> would
// produce Arc<Arc<...>> whose HttpClient bound is unsatisfied.
type DidR = CommonDidResolver<VitaHttpClient>;
type HandleR = AppViewHandleResolver<VitaHttpClient>;
type InnerOAuthClient =
    OAuthClient<CapturingStateStore, FileOAuthSessionStore, DidR, HandleR, VitaHttpClient>;

/// Which redirect URI the client should use for this flow.
/// `Broker` is the v1 default; `Qr` is reserved for v1.x camera-scan pickup.
#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Broker,
    #[allow(dead_code)] // wired up in v1.x
    Qr,
}

impl Transport {
    fn redirect_uri(self) -> &'static str {
        match self {
            Transport::Broker => REDIRECT_URI_BROKER,
            Transport::Qr => REDIRECT_URI_QR,
        }
    }
}

/// Single error type for everything the OAuth layer can fail at.
#[derive(Debug)]
pub enum OAuthError {
    Construct(String),
    Authorize(String),
    Callback(String),
    Restore(String),
    NoPersistedSession,
    SessionStore(OAuthSessionStoreError),
    Resolver(atrium_identity::Error),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthError::Construct(m) => write!(f, "oauth client construct: {m}"),
            OAuthError::Authorize(m) => write!(f, "oauth authorize: {m}"),
            OAuthError::Callback(m) => write!(f, "oauth callback: {m}"),
            OAuthError::Restore(m) => write!(f, "oauth restore: {m}"),
            OAuthError::NoPersistedSession => write!(f, "no persisted oauth session"),
            OAuthError::SessionStore(e) => write!(f, "oauth session store: {e}"),
            OAuthError::Resolver(e) => write!(f, "identity resolver: {e}"),
        }
    }
}

impl std::error::Error for OAuthError {}

impl From<OAuthSessionStoreError> for OAuthError {
    fn from(e: OAuthSessionStoreError) -> Self {
        OAuthError::SessionStore(e)
    }
}

impl From<atrium_identity::Error> for OAuthError {
    fn from(e: atrium_identity::Error) -> Self {
        OAuthError::Resolver(e)
    }
}

/// Opaque handle returned from [`start_flow`] to bind a flow's state to
/// its completion. Just carries the state string for now; the OAuthClient
/// (with its in-memory state_store) lives on the [`VitaOAuthClient`] that
/// the caller holds.
#[derive(Clone, Debug)]
pub struct PendingFlow {
    pub state: String,
}

/// The successful result of a completed OAuth flow (or a successful resume).
/// The caller wraps this into `bsky_auth::AuthClient` with
/// `AuthAgent::OAuth(result.agent)`.
pub struct OAuthLoginResult {
    pub agent: OAuthAgent,
    pub resolved: ResolvedIdentity,
}

/// Vita-side OAuth client. Holds an `atrium_oauth::OAuthClient` configured
/// with our concrete transport + resolvers + persistence. Cheap to construct
/// (~one identity-resolver setup + a file read for the existing session).
pub struct VitaOAuthClient {
    inner: Arc<InnerOAuthClient>,
    state_store: Arc<CapturingStateStore>,
    /// Separate `VitaHttpClient` instance held in an `Arc` for ad-hoc DID
    /// resolution at session-complete and resume time. atrium-oauth already
    /// owns its own internal `VitaHttpClient` (passed by value to
    /// `OAuthClientConfig.http_client`), but we can't extract a shared
    /// reference back out — keeping our own `Arc<VitaHttpClient>` here is
    /// cheap (ureq::Agent is internally Arc'd) and avoids the wrapper
    /// dance.
    http_for_resolve: Arc<VitaHttpClient>,
}

impl VitaOAuthClient {
    pub fn new() -> Result<Self, OAuthError> {
        let http_for_resolve = Arc::new(VitaHttpClient::new());

        // Identity resolvers used INSIDE atrium-oauth for handle→DID→PDS during
        // authorize/callback. atrium-oauth wraps these into its own OAuthResolver
        // chain (with caching + throttling). We don't reference these post-config.
        let oauth_did_resolver = CommonDidResolver::new(CommonDidResolverConfig {
            plc_directory_url: DEFAULT_PLC_DIRECTORY_URL.to_string(),
            http_client: Arc::clone(&http_for_resolve),
        });
        let oauth_handle_resolver = AppViewHandleResolver::new(AppViewHandleResolverConfig {
            service_url: HANDLE_RESOLVER_URL.to_string(),
            http_client: Arc::clone(&http_for_resolve),
        });

        let state_store = Arc::new(CapturingStateStore::new());
        let session_store = FileOAuthSessionStore::new(OAUTH_SESSION_PATH);

        let client_metadata = AtprotoClientMetadata {
            client_id: CLIENT_METADATA_URL.to_string(),
            client_uri: Some("https://www.davidlewis.xyz/bsky-vita/".to_string()),
            redirect_uris: vec![
                REDIRECT_URI_BROKER.to_string(),
                REDIRECT_URI_QR.to_string(),
            ],
            token_endpoint_auth_method: AuthMethod::None,
            grant_types: vec![GrantType::AuthorizationCode, GrantType::RefreshToken],
            scopes: vec![
                Scope::Known(KnownScope::Atproto),
                Scope::Known(KnownScope::TransitionGeneric),
                Scope::Known(KnownScope::TransitionChatBsky),
            ],
            jwks_uri: None,
            token_endpoint_auth_signing_alg: None,
        };

        let config = OAuthClientConfig {
            client_metadata,
            keys: None,
            state_store: (*state_store).clone(),
            session_store,
            resolver: OAuthResolverConfig {
                did_resolver: oauth_did_resolver,
                handle_resolver: oauth_handle_resolver,
                authorization_server_metadata: Default::default(),
                protected_resource_metadata: Default::default(),
            },
            // By VALUE — atrium-oauth Arcs internally.
            http_client: VitaHttpClient::new(),
        };
        let inner = OAuthClient::new(config).map_err(|e| OAuthError::Construct(format!("{e}")))?;

        Ok(Self {
            inner: Arc::new(inner),
            state_store,
            http_for_resolve,
        })
    }

    /// Begin an OAuth flow. PAR + identity resolution + state generation
    /// all happen here. Returns the authorize URL (display as QR) and the
    /// `state` value (poll the broker with this).
    pub fn start_flow(
        &self,
        handle_or_did: &str,
        transport: Transport,
    ) -> Result<(String /* auth_url */, PendingFlow), OAuthError> {
        let options = AuthorizeOptions {
            redirect_uri: Some(transport.redirect_uri().to_string()),
            scopes: vec![
                Scope::Known(KnownScope::Atproto),
                Scope::Known(KnownScope::TransitionGeneric),
                Scope::Known(KnownScope::TransitionChatBsky),
            ],
            prompt: None,
            state: None, // app-state; not the OAuth `state` nonce
        };
        // Clear any prior state capture before authorize() so we read the
        // value generated by THIS call.
        self.state_store.clear_capture();
        let url = block_on(self.inner.authorize(handle_or_did, options))
            .map_err(|e| OAuthError::Authorize(format!("{e}")))?;
        let state = self
            .state_store
            .last_captured()
            .ok_or_else(|| OAuthError::Authorize("state was not captured (atrium-oauth API change?)".into()))?;
        bsky_log::log!(
            "oauth: start_flow handle={} state={} url_len={}",
            handle_or_did,
            state,
            url.len()
        );
        Ok((url, PendingFlow { state }))
    }

    /// Complete an OAuth flow with the `code` retrieved from the broker.
    /// `state` must match what [`start_flow`] returned for this client.
    /// `iss` is the issuer identifier the auth server sent in the redirect.
    pub fn complete_flow(
        &self,
        pending: PendingFlow,
        code: &str,
        iss: &str,
    ) -> Result<OAuthLoginResult, OAuthError> {
        let params = CallbackParams {
            code: code.to_string(),
            state: Some(pending.state),
            iss: Some(iss.to_string()),
        };
        let (session, _app_state) = block_on(self.inner.callback(params))
            .map_err(|e| OAuthError::Callback(format!("{e}")))?;
        // `OAuthSession` implements `SessionManager`; pull the DID it was
        // issued for.
        use atrium_api::agent::SessionManager;
        let sub = block_on(session.did())
            .ok_or_else(|| OAuthError::Callback("session has no DID".into()))?;
        let resolved = self.resolve_identity_for_did(&sub)?;
        Ok(OAuthLoginResult {
            agent: Agent::new(session),
            resolved,
        })
    }

    fn resolve_identity_for_did(&self, did: &Did) -> Result<ResolvedIdentity, OAuthError> {
        let did_resolver = CommonDidResolver::new(CommonDidResolverConfig {
            plc_directory_url: DEFAULT_PLC_DIRECTORY_URL.to_string(),
            http_client: Arc::clone(&self.http_for_resolve),
        });
        let doc: DidDocument = block_on(did_resolver.resolve(did))?;
        let pds = doc
            .service
            .as_ref()
            .and_then(|svcs| {
                svcs.iter()
                    .find(|s| s.r#type == "AtprotoPersonalDataServer")
                    .map(|s| s.service_endpoint.clone())
            })
            .ok_or_else(|| {
                OAuthError::Callback("DID doc has no AtprotoPersonalDataServer service".into())
            })?;
        Ok(ResolvedIdentity {
            did: did.as_str().to_string(),
            pds,
        })
    }
}

/// Build a fresh `VitaOAuthClient`, check if a persisted OAuth session
/// exists, and if so call `OAuthClient::restore(did)` to revive it.
/// Returns `Ok(None)` for "no persisted session" so the caller can fall
/// through to the app-password resume path or the login form.
pub fn try_resume_existing_oauth_session() -> Result<Option<OAuthLoginResult>, OAuthError> {
    // Probe disk first without standing up the full OAuthClient, to keep
    // the no-session case cheap.
    let probe = FileOAuthSessionStore::new(OAUTH_SESSION_PATH);
    let Some(did) = probe.get_persisted_did() else {
        return Ok(None);
    };
    drop(probe); // release the file handle / Mutex; new client opens its own.

    let client = VitaOAuthClient::new()?;
    let session = block_on(client.inner.restore(&did))
        .map_err(|e| OAuthError::Restore(format!("{e}")))?;
    let resolved = client.resolve_identity_for_did(&did)?;
    Ok(Some(OAuthLoginResult {
        agent: Agent::new(session),
        resolved,
    }))
}

/// Free-function convenience for callers that don't want to hold a
/// [`VitaOAuthClient`] themselves. **Note** the same client must be used
/// for `start_flow` and `complete_flow` (the in-memory state store binds
/// them) — these free functions are only ergonomic when the LoginScreen
/// holds a `VitaOAuthClient` in its state and calls the methods directly.
#[allow(dead_code)]
pub fn start_flow(
    client: &VitaOAuthClient,
    handle: &str,
    transport: Transport,
) -> Result<(String, PendingFlow), OAuthError> {
    client.start_flow(handle, transport)
}

#[allow(dead_code)]
pub fn complete_flow(
    client: &VitaOAuthClient,
    pending: PendingFlow,
    code: &str,
    iss: &str,
) -> Result<OAuthLoginResult, OAuthError> {
    client.complete_flow(pending, code, iss)
}

// ─────────────────────────── State store ────────────────────────────────

/// Wraps an in-memory state store and captures the most-recently-set key,
/// so `start_flow` can read the random `state` value atrium-oauth
/// generated during `authorize()`.
#[derive(Clone)]
pub(crate) struct CapturingStateStore {
    inner: Arc<atrium_common::store::memory::MemoryStore<String, InternalStateData>>,
    last_captured: Arc<Mutex<Option<String>>>,
}

impl CapturingStateStore {
    fn new() -> Self {
        Self {
            inner: Arc::new(atrium_common::store::memory::MemoryStore::default()),
            last_captured: Arc::new(Mutex::new(None)),
        }
    }
    fn clear_capture(&self) {
        if let Ok(mut g) = self.last_captured.lock() {
            *g = None;
        }
    }
    fn last_captured(&self) -> Option<String> {
        self.last_captured.lock().ok().and_then(|g| g.clone())
    }
}

impl Store<String, InternalStateData> for CapturingStateStore {
    type Error = atrium_common::store::memory::Error;

    fn get(
        &self,
        key: &String,
    ) -> impl Future<Output = Result<Option<InternalStateData>, Self::Error>> + Send {
        self.inner.get(key)
    }

    fn set(
        &self,
        key: String,
        value: InternalStateData,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        // Capture before delegating; the value is set-once per state and the
        // capture is overwritten on every call (caller calls clear_capture()
        // before authorize() to avoid stale reads).
        if let Ok(mut g) = self.last_captured.lock() {
            *g = Some(key.clone());
        }
        self.inner.set(key, value)
    }

    fn del(&self, key: &String) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.del(key)
    }

    fn clear(&self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.clear()
    }
}

impl StateStore for CapturingStateStore {}

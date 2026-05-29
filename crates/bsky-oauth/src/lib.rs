//! AT Protocol OAuth (PAR + PKCE + DPoP) for bsky-vita.
//!
//! Wraps `atrium-oauth` 0.1 with the Vita-side concrete types:
//! - `Arc<bsky_net::VitaHttpClient>` as the HTTP transport (rustls+ring+webpki-roots).
//! - `atrium_identity` resolvers (`CommonDidResolver` + `AppViewHandleResolver`).
//! - In-memory state store (in-flight PKCE+DPoP state, lost across app restarts).
//! - File-backed session store at `ux0:data/BSKY00001/auth/oauth-session.json`.
//!
//! ## Flow
//!
//! 1. [`start_flow`] — calls PAR via [`atrium_oauth::OAuthClient::authorize`],
//!    returns the authorize URL (for QR display) + the `state` value (for
//!    broker polling) + an opaque [`PendingFlow`] handle the caller holds
//!    until step 3.
//! 2. The caller (LoginScreen) renders the URL as a QR; user scans + consents
//!    on their phone; the broker receives `(code, state, iss)` and the Vita
//!    polls `/pop?state=...` until it picks them up.
//! 3. [`complete_flow`] — calls [`atrium_oauth::OAuthClient::callback`],
//!    exchanges code for tokens, persists the session, and produces an
//!    [`bsky_auth::AuthClient`] wrapping [`bsky_auth::AuthAgent::OAuth`].
//! 4. [`try_resume_existing_oauth_session`] — on app launch, looks up a
//!    persisted DID in the session store and calls `OAuthClient::restore`
//!    to rebuild a live `OAuthSession` (refreshes access tokens as needed).

use atrium_identity::did::CommonDidResolver;
use atrium_identity::handle::AppViewHandleResolver;
use bsky_net::VitaHttpClient;

pub mod broker;
pub mod client;
pub mod session_store;

pub use broker::{spawn_broker_poll, PollOutcome};
pub use client::{
    complete_flow, start_flow, try_resume_existing_oauth_session, OAuthError, OAuthLoginResult,
    PendingFlow, Transport, VitaOAuthClient,
};
pub use session_store::{FileOAuthSessionStore, OAuthSessionStoreError};

/// Origin of the Cloudflare Worker that hosts OAuth metadata + receives
/// the callback + serves the /pop poll endpoint. Default deployment lives
/// at the URL below; users who self-host the Worker (`broker/`) point this
/// at their own custom domain before building their VPK.
pub const BROKER_ORIGIN: &str = "https://broker.davidlewis.xyz";

/// Hosted client_metadata.json URL. The atproto authorization server fetches
/// this to learn our redirect URIs, scopes, etc. Served by the Worker
/// itself (folded in from a separate static host so the OAuth surface is
/// a single deployable). The `client_id` field in the metadata MUST equal
/// this URL — the AS treats it as the canonical identifier.
pub const CLIENT_METADATA_URL: &str =
    "https://broker.davidlewis.xyz/client_metadata.json";

/// Broker pickup redirect URI. Declared in `client_metadata.json`.
/// Must match exactly (byte-for-byte) per the OAuth spec. No trailing
/// slash because the Worker routes `/callback` exactly via its switch.
pub const REDIRECT_URI_BROKER: &str = "https://broker.davidlewis.xyz/callback";

/// QR-pickup redirect URI. v1.x; declared from v1 so adding QR later
/// triggers no metadata churn or re-consent.
pub const REDIRECT_URI_QR: &str = "https://broker.davidlewis.xyz/callback-qr";

/// Broker `/pop` endpoint. The Vita polls this with its own `state` value
/// to retrieve the OAuth code once the user's phone has consented.
pub const BROKER_POP_URL: &str = "https://broker.davidlewis.xyz/pop";

/// Where the persisted OAuth session lives on the Vita. Separate from the
/// app-password `session.json` so both auth paths can coexist.
pub const OAUTH_SESSION_PATH: &str = "ux0:data/BSKY00001/auth/oauth-session.json";

/// Public AppView used as the XRPC handle-resolver fallback. Same as
/// `bsky-auth`'s constant; duplicated here to avoid the dep cycle.
pub const HANDLE_RESOLVER_URL: &str = "https://public.api.bsky.app";

/// Concrete handle for the OAuth-backed `Agent` that lives inside
/// `AuthAgent::OAuth`. The type is verbose; the alias keeps `bsky_auth`'s
/// enum declaration readable.
///
/// `T = VitaHttpClient` (by value) because `atrium-oauth` `Arc`s the HTTP
/// client internally — passing `Arc<VitaHttpClient>` would land us with
/// `Arc<Arc<VitaHttpClient>>` whose `Arc<T>: HttpClient` bound is not
/// satisfied (no blanket impl). The resolvers' `T` parameter is the same
/// — they hold an `Arc<T>` internally but the type-level `T` is the raw
/// `VitaHttpClient`.
pub type OAuthAgent = atrium_api::agent::Agent<
    atrium_oauth::OAuthSession<
        VitaHttpClient,
        CommonDidResolver<VitaHttpClient>,
        AppViewHandleResolver<VitaHttpClient>,
        FileOAuthSessionStore,
    >,
>;

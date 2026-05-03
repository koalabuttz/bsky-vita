//! Bluesky authentication for the Vita.
//!
//! Wraps `atrium-api`'s `AtpAgent` with three Vita-specific pieces:
//!
//! - [`xrpc::PdsClient`] — an `XrpcClient` that pairs our [`bsky_net::VitaHttpClient`]
//!   with a PDS base URL. Different users live on different PDSes (bsky.social,
//!   yapfest.club, etc.), so the base URL is determined at login time, not compile time.
//! - [`store::FileSessionStore`] — persists `AtpSession` to
//!   `ux0:data/BSKY00001/auth/session.json` with a tmp-and-rename atomic write.
//!   Implements `Store<(), AtpSession>` + `AuthorizationProvider` + `AtpSessionStore`.
//! - [`resolver::resolve_pds`] — handle → DID → DID document → PDS URL via
//!   `atrium-identity`'s `AppViewHandleResolver` + `CommonDidResolver`. No DNS-TXT
//!   path; the XRPC fallback (`com.atproto.identity.resolveHandle` against
//!   `public.api.bsky.app`) covers every handle including custom-domain ones.
//!
//! ## Drift between sync and async
//!
//! Atrium's traits return `impl Future + Send`. Our backing I/O is synchronous
//! (file ops, ureq). We wrap sync bodies in `async move {}` to satisfy the
//! signature. Consumers (typically `app/main.rs`) drive the resulting top-level
//! future with `futures::executor::block_on`. There is no tokio runtime; the
//! `tokio::sync::Mutex`/`Notify` primitives that atrium uses internally are
//! runtime-agnostic.

pub mod credentials;
pub mod resolver;
pub mod store;
pub mod xrpc;

pub use credentials::{load_credentials, Credentials};
pub use resolver::{resolve_pds, ResolvedIdentity};
pub use store::{FileSessionStore, SessionStoreError};
pub use xrpc::PdsClient;

/// Default app-data directory. Written to from [`store::FileSessionStore`] and
/// read by [`credentials::load_credentials`]. Matches our TITLEID `BSKY00001`.
pub const DATA_DIR: &str = "ux0:data/BSKY00001";

/// Default session file path within [`DATA_DIR`].
pub const SESSION_PATH: &str = "ux0:data/BSKY00001/auth/session.json";

/// Default credentials file path within [`DATA_DIR`].
pub const CREDENTIALS_PATH: &str = "ux0:data/BSKY00001/credentials.toml";

/// Public AppView used as the XRPC fallback for handle resolution. Works for
/// any handle (bsky.social, custom domains, custom PDS users) because the
/// AppView indexes the firehose globally.
pub const HANDLE_RESOLVER_URL: &str = "https://public.api.bsky.app";

/// Single error type covering anything the auth layer can fail at.
#[derive(Debug)]
pub enum AuthError {
    Identity(atrium_identity::Error),
    Login(String),
    Session(SessionStoreError),
    Credentials(String),
    Io(std::io::Error),
    Other(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Identity(e) => write!(f, "identity resolution failed: {e}"),
            AuthError::Login(msg) => write!(f, "login failed: {msg}"),
            AuthError::Session(e) => write!(f, "session store error: {e}"),
            AuthError::Credentials(msg) => write!(f, "credentials error: {msg}"),
            AuthError::Io(e) => write!(f, "io error: {e}"),
            AuthError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AuthError::Identity(e) => Some(e),
            AuthError::Session(e) => Some(e),
            AuthError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AuthError {
    fn from(e: std::io::Error) -> Self {
        AuthError::Io(e)
    }
}

impl From<SessionStoreError> for AuthError {
    fn from(e: SessionStoreError) -> Self {
        AuthError::Session(e)
    }
}

impl From<atrium_identity::Error> for AuthError {
    fn from(e: atrium_identity::Error) -> Self {
        AuthError::Identity(e)
    }
}

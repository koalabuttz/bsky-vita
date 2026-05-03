//! Handle → DID → DID document → PDS URL.
//!
//! Configures `atrium-identity` to use only the XRPC fallback for handle
//! resolution (`com.atproto.identity.resolveHandle` against
//! `public.api.bsky.app`), since DNS-TXT is unreliable on the Vita and the
//! AppView resolver works for any handle including custom-domain ones. The
//! DID resolver is the standard `CommonDidResolver` (handles both `did:plc`
//! via `https://plc.directory` and `did:web` via `/.well-known/did.json`).

pub use atrium_identity::identity_resolver::ResolvedIdentity;

use atrium_common::resolver::Resolver;
use atrium_identity::did::{
    CommonDidResolver, CommonDidResolverConfig, DEFAULT_PLC_DIRECTORY_URL,
};
use atrium_identity::handle::{AppViewHandleResolver, AppViewHandleResolverConfig};
use atrium_identity::identity_resolver::{IdentityResolver, IdentityResolverConfig};
use bsky_net::VitaHttpClient;
use std::sync::Arc;

use crate::{AuthError, HANDLE_RESOLVER_URL};

/// Resolve a handle (or DID) to its current PDS endpoint. Called once at login.
///
/// Input may be:
///   - `david.yapfest.club` (handle on a custom-PDS host)
///   - `alice.bsky.social` (handle on Bluesky's PDS)
///   - `did:plc:abc123...` or `did:web:foo.com` (already-resolved DIDs; we
///     skip the handle step)
///
/// Returns the canonical DID and PDS URL the user's account is currently on.
pub async fn resolve_pds(
    http_client: Arc<VitaHttpClient>,
    handle_or_did: &str,
) -> Result<ResolvedIdentity, AuthError> {
    let did_resolver = CommonDidResolver::new(CommonDidResolverConfig {
        plc_directory_url: DEFAULT_PLC_DIRECTORY_URL.to_string(),
        http_client: Arc::clone(&http_client),
    });
    let handle_resolver = AppViewHandleResolver::new(AppViewHandleResolverConfig {
        service_url: HANDLE_RESOLVER_URL.to_string(),
        http_client,
    });
    let resolver = IdentityResolver::new(IdentityResolverConfig {
        did_resolver,
        handle_resolver,
    });
    Ok(resolver.resolve(handle_or_did).await?)
}

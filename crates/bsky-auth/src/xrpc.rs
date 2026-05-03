//! `XrpcClient` over [`bsky_net::VitaHttpClient`] + a per-PDS base URI.
//!
//! Atrium's `XrpcClient` trait extends `HttpClient` with a `base_uri()`
//! method (and several optional headers). The base URI is the user's PDS
//! (e.g. `https://bsky.social` or `https://yapfest.club`), which we don't
//! know until the identity resolver returns it.

use atrium_xrpc::http::{Request, Response};
use atrium_xrpc::{HttpClient, XrpcClient};
use bsky_net::VitaHttpClient;
use std::error::Error;
use std::sync::{Arc, Mutex};

/// Wraps a [`VitaHttpClient`] with a mutable base URI. The URI is set once at
/// login time and rarely changes; `Mutex` allows atrium's `Configure` machinery
/// to update it (e.g. when the DID document is parsed during login and points
/// at a different endpoint than what we used to resolve).
pub struct PdsClient {
    inner: Arc<VitaHttpClient>,
    base_uri: Mutex<String>,
}

impl PdsClient {
    pub fn new(inner: Arc<VitaHttpClient>, base_uri: impl Into<String>) -> Self {
        Self { inner, base_uri: Mutex::new(base_uri.into()) }
    }

    pub fn set_base_uri(&self, uri: impl Into<String>) {
        *self.base_uri.lock().expect("base_uri mutex poisoned") = uri.into();
    }
}

impl HttpClient for PdsClient {
    fn send_http(
        &self,
        request: Request<Vec<u8>>,
    ) -> impl std::future::Future<
        Output = Result<Response<Vec<u8>>, Box<dyn Error + Send + Sync + 'static>>,
    > + Send {
        let inner = Arc::clone(&self.inner);
        async move { inner.send_http(request).await }
    }
}

impl XrpcClient for PdsClient {
    fn base_uri(&self) -> String {
        self.base_uri.lock().expect("base_uri mutex poisoned").clone()
    }
}

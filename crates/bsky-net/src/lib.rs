//! XRPC HTTP client for the Vita target.
//!
//! Implements [`atrium_xrpc::HttpClient`] over our sync `ureq` + `rustls` +
//! `ring` (vita-rust patch) + `webpki-roots` stack. The trait's `send_http`
//! returns `impl Future<Output = ...> + Send`; the body of our impl runs
//! synchronously inside an `async move` block, so the type signature is
//! satisfied without bringing in tokio or any other async runtime.
//!
//! Consumers drive the returned future with [`futures::executor::block_on`]
//! (or any other executor that can poll a single Send-bound future).

use std::error::Error;
use std::io::Read;

use atrium_xrpc::http::{Request, Response};
use atrium_xrpc::HttpClient;

/// A blocking HTTP client suitable for the Vita target.
///
/// Wraps a `ureq::Agent` (which is internally `Arc`-shared, so cloning is
/// cheap and lets us hand a copy into the per-request `async move` block).
pub struct VitaHttpClient {
    agent: ureq::Agent,
}

impl Default for VitaHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl VitaHttpClient {
    pub fn new() -> Self {
        // Inactivity timeouts, NOT an overall deadline. `timeout_read` /
        // `timeout_write` bound how long a single socket op may stall with
        // no progress; as long as bytes keep flowing the clock resets. A
        // previous overall `.timeout(45s)` killed large-but-progressing
        // downloads (a 20 MB video on slow wifi exceeds 45s even while
        // steadily transferring) — fatal for video blobs, which are read
        // whole into memory. With inactivity timeouts a slow steady
        // download finishes; only a genuinely stalled connection errors.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(30))
            .timeout_write(std::time::Duration::from_secs(30))
            .build();
        Self { agent }
    }

    /// GET `url` and read the whole body into memory, invoking
    /// `on_progress(downloaded, total)` as bytes arrive (`total` is the
    /// `Content-Length` if the server sent one, else `None`). Synchronous
    /// (ureq is blocking) and auth-free — used by the worker's video-blob
    /// download to drive the "Loading video…" progress bar. Reads in 64 KB
    /// chunks; the caller is expected to throttle `on_progress` if it
    /// forwards updates over a channel.
    pub fn get_with_progress(
        &self,
        url: &str,
        mut on_progress: impl FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        let resp = match self.agent.get(url).call() {
            Ok(r) => r,
            // ureq classifies HTTP >= 400 as Err(Status); surface the code.
            Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code}")),
            Err(e) => return Err(format!("{e}")),
        };
        let total: Option<u64> = resp
            .header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok());
        let mut reader = resp.into_reader();
        let mut out: Vec<u8> = Vec::new();
        if let Some(t) = total {
            out.reserve(t.min(64 * 1024 * 1024) as usize);
        }
        let mut buf = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    on_progress(out.len() as u64, total);
                }
                Err(e) => return Err(format!("read: {e}")),
            }
        }
        Ok(out)
    }
}

impl HttpClient for VitaHttpClient {
    fn send_http(
        &self,
        request: Request<Vec<u8>>,
    ) -> impl std::future::Future<
        Output = Result<Response<Vec<u8>>, Box<dyn Error + Send + Sync + 'static>>,
    > + Send {
        let agent = self.agent.clone();
        async move {
            let (parts, body) = request.into_parts();
            let url = parts.uri.to_string();
            let method = parts.method.as_str();

            let mut req = agent.request(method, &url);
            for (name, value) in parts.headers.iter() {
                req = req.set(name.as_str(), value.to_str()?);
            }

            // ureq classifies HTTP >=400 as `Err(Status)`, but we want to pass
            // those through as a normal `Response` — atrium handles 401 via
            // its session-refresh path and other 4xx/5xx bubble up to callers
            // as XRPC errors with the body intact.
            let resp = match if body.is_empty() {
                req.call()
            } else {
                req.send_bytes(&body)
            } {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r,
                Err(e) => return Err(Box::new(e) as Box<dyn Error + Send + Sync + 'static>),
            };

            let status = resp.status();
            let mut builder = Response::builder().status(status);
            for name in resp.headers_names() {
                if let Some(value) = resp.header(&name) {
                    builder = builder.header(name, value);
                }
            }

            let mut body_bytes = Vec::new();
            resp.into_reader().read_to_end(&mut body_bytes)?;
            Ok(builder.body(body_bytes)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_constructs() {
        let _ = VitaHttpClient::new();
    }

    #[test]
    fn client_is_send_sync() {
        // Confirms the resulting future is Send-bound, which atrium requires.
        fn assert_send<T: Send>(_: T) {}
        let client = VitaHttpClient::new();
        let req = Request::builder()
            .method("GET")
            .uri("https://example.invalid/")
            .body(Vec::new())
            .unwrap();
        let fut = client.send_http(req);
        assert_send(fut);
    }
}

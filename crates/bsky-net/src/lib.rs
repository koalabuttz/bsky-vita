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
use std::io::{Read, Write};

use atrium_xrpc::http::{Request, Response};
use atrium_xrpc::HttpClient;

/// Hard ceilings on response sizes, enforced by the capped readers below.
/// The Vita shares ~512 MB of RAM with the OS, so an unbounded body from a
/// hostile or buggy server is a real OOM vector — every read is capped.
/// These are coarse safety ceilings, not tight per-asset budgets: finer
/// per-type limits (avatar vs thumb vs full image) would require threading an
/// asset kind through the worker's `FetchImage` request and touching every UI
/// call site, which is deliberately out of scope here.
pub const MAX_XRPC_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: u64 = 24 * 1024 * 1024;
pub const MAX_VIDEO_BYTES: u64 = 120 * 1024 * 1024;

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

    /// GET `url` into memory, capped at `max_bytes`. Synchronous (ureq
    /// blocks) and auth-free — used for image fetches against the CDN.
    /// Returns an error rather than OOMing if the server advertises (via
    /// Content-Length) or streams more than `max_bytes`.
    pub fn get_bytes_capped(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        let resp = match self.agent.get(url).call() {
            Ok(r) => r,
            // ureq classifies HTTP >= 400 as Err(Status); surface the code.
            Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code}")),
            Err(e) => return Err(format!("{e}")),
        };
        let total = content_length(&resp);
        read_capped(resp.into_reader(), max_bytes, total, |_, _| {})
    }

    /// Stream `url` to `dest`, capped at `max_bytes`, forwarding throttled
    /// progress via `on_progress(downloaded, total)`. The body is written
    /// straight to a `<dest>.part` sidecar in 64 KB chunks — it never sits
    /// whole in memory — and renamed to `dest` only after a complete,
    /// within-cap download. The partial file is removed on any error, so a
    /// half-written file is never mistaken for a complete cache entry.
    /// Synchronous and auth-free; used for video blobs (tens of MB).
    pub fn download_to_file_with_progress(
        &self,
        url: &str,
        dest: &std::path::Path,
        max_bytes: u64,
        on_progress: impl FnMut(u64, Option<u64>),
    ) -> Result<(), String> {
        let resp = match self.agent.get(url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code}")),
            Err(e) => return Err(format!("{e}")),
        };
        let total = content_length(&resp);
        if let Some(t) = total {
            if t > max_bytes {
                return Err(format!("body too large: Content-Length {t} > cap {max_bytes}"));
            }
        }
        // Stream straight to disk via a `.part` sidecar, capped, then install.
        stream_to_file_capped(resp.into_reader(), dest, max_bytes, total, on_progress)
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

            // Cap XRPC response bodies — a runaway or hostile response must
            // not be read unbounded into memory on a 512 MB device.
            let total = content_length(&resp);
            let body_bytes = read_capped(resp.into_reader(), MAX_XRPC_BYTES, total, |_, _| {})
                .map_err(Box::<dyn Error + Send + Sync + 'static>::from)?;
            Ok(builder.body(body_bytes)?)
        }
    }
}

/// `Content-Length` header as a `u64`, if present and parseable.
fn content_length(resp: &ureq::Response) -> Option<u64> {
    resp.header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
}

/// Read `reader` fully into a `Vec`, but never more than `max_bytes`.
/// Rejects up front if `total` (Content-Length) already exceeds the cap, and
/// aborts mid-stream if the body grows past it — so a missing or lying
/// Content-Length cannot lead to unbounded allocation. Forwards
/// `on_progress(downloaded, total)` as bytes arrive.
fn read_capped(
    mut reader: impl Read,
    max_bytes: u64,
    total: Option<u64>,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, String> {
    if let Some(t) = total {
        if t > max_bytes {
            return Err(format!("body too large: Content-Length {t} > cap {max_bytes}"));
        }
    }
    let mut out: Vec<u8> = Vec::new();
    if let Some(t) = total {
        out.reserve(t.min(max_bytes) as usize);
    }
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() as u64 + n as u64 > max_bytes {
                    return Err(format!("body exceeded cap of {max_bytes} bytes"));
                }
                out.extend_from_slice(&buf[..n]);
                on_progress(out.len() as u64, total);
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Ok(out)
}

/// Stream `reader` to `dest` via a `<dest>.part` sidecar, capped at
/// `max_bytes`, forwarding `on_progress(downloaded, total)`. On success the
/// sidecar is renamed to `dest`, removing any existing `dest` first — Vita's
/// filesystem doesn't reliably rename over an existing file (mirrors
/// `bsky_auth`'s `FileSessionStore::write_to_disk`), and this also clears a
/// stale zero-byte cache file the caller's cache-hit check skipped. The `.part`
/// is removed on any error so a torn download never lingers.
fn stream_to_file_capped(
    mut reader: impl Read,
    dest: &std::path::Path,
    max_bytes: u64,
    total: Option<u64>,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    // `<dest>.part` (append, don't replace the extension — keeps the `.mp4` so
    // the sidecar reads as `<cid>.mp4.part`).
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);

    let mut stream = || -> Result<(), String> {
        let mut file = std::fs::File::create(&part)
            .map_err(|e| format!("create {}: {e}", part.display()))?;
        let mut buf = [0u8; 64 * 1024];
        let mut written: u64 = 0;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => return Err(format!("read: {e}")),
            };
            written += n as u64;
            if written > max_bytes {
                return Err(format!("body exceeded cap of {max_bytes} bytes"));
            }
            file.write_all(&buf[..n])
                .map_err(|e| format!("write {}: {e}", part.display()))?;
            on_progress(written, total);
        }
        file.flush().map_err(|e| format!("flush {}: {e}", part.display()))
    };

    match stream() {
        Ok(()) => {
            // Vita won't reliably rename over an existing file — remove a stale
            // destination (e.g. a zero-byte `<cid>.mp4` from an earlier failed
            // write) before installing the freshly downloaded `.part`.
            if dest.exists() {
                if let Err(e) = std::fs::remove_file(dest) {
                    let _ = std::fs::remove_file(&part);
                    return Err(format!("remove stale {}: {e}", dest.display()));
                }
            }
            std::fs::rename(&part, dest).map_err(|e| {
                let _ = std::fs::remove_file(&part);
                format!("rename {} -> {}: {e}", part.display(), dest.display())
            })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_capped_rejects_oversized_content_length() {
        // Server advertises 10 bytes, cap is 4 → fast-reject before reading.
        let r = read_capped(std::io::Cursor::new(vec![0u8; 10]), 4, Some(10), |_, _| {});
        assert!(r.is_err());
    }

    #[test]
    fn read_capped_rejects_stream_over_cap_without_content_length() {
        // No Content-Length, but the stream itself exceeds the cap → abort
        // mid-read rather than allocate unbounded.
        let r = read_capped(std::io::Cursor::new(vec![0u8; 100]), 16, None, |_, _| {});
        assert!(r.is_err());
    }

    #[test]
    fn read_capped_accepts_within_cap() {
        let data = vec![7u8; 50];
        let out = read_capped(std::io::Cursor::new(data.clone()), 64, Some(50), |_, _| {}).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn stream_to_file_overwrites_stale_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("vid.mp4");
        // A stale zero-byte cache file (the kind the cache-hit check skips).
        std::fs::write(&dest, b"").unwrap();
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 0);

        let data = vec![9u8; 2048];
        stream_to_file_capped(
            std::io::Cursor::new(data.clone()),
            &dest,
            1 << 20,
            Some(data.len() as u64),
            |_, _| {},
        )
        .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), data, "stale file replaced");
        let mut part = dest.clone().into_os_string();
        part.push(".part");
        assert!(!std::path::Path::new(&part).exists(), ".part cleaned up");
    }

    #[test]
    fn stream_to_file_over_cap_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("vid.mp4");
        let r = stream_to_file_capped(
            std::io::Cursor::new(vec![0u8; 100]),
            &dest,
            16,
            None,
            |_, _| {},
        );
        assert!(r.is_err(), "over-cap stream rejected");
        assert!(!dest.exists(), "no final file on cap failure");
        let mut part = dest.into_os_string();
        part.push(".part");
        assert!(!std::path::Path::new(&part).exists(), "no .part left behind");
    }

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

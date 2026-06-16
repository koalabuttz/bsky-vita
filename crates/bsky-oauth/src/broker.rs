//! Background thread that polls the Cloudflare Worker broker for the
//! OAuth `(code, iss)` payload after the user consents on their phone.
//!
//! Lives outside the main `bsky_worker::Worker` because LoginScreen runs
//! before any [`bsky_auth::AuthClient`] exists, and the main Worker is
//! authentication-bound (its thread is constructed with `Arc<AuthClient>`).
//! This polling thread is unauthenticated — it makes a plain HTTPS GET
//! against the broker — so spawning it inline from LoginScreen is fine.
//!
//! The thread sends progress via a [`std::sync::mpsc::Receiver`] the
//! caller drains each frame. On success ([`PollOutcome::Ready`]) the
//! caller advances to the code-exchange step. On timeout / fatal error
//! ([`PollOutcome::Failed`]) it surfaces a message and returns to the
//! login form.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Deserialize;

use crate::BROKER_POP_URL;

/// Total wall-clock time to wait for the user to consent on their phone.
/// Bluesky's authorize page session also expires (~5 min); matching here.
const POLL_DEADLINE_SECS: u64 = 300;

/// Sleep between successive `/pop` polls. Keeps below KV eventual-
/// consistency latency (~few seconds) without hammering the Worker.
const POLL_INTERVAL_SECS: u64 = 2;

/// Granularity of the inter-poll sleep, in milliseconds. The 2 s wait
/// between polls is split into this many short slices so the cancel flag
/// is observed within ~one slice instead of up to a full `POLL_INTERVAL_SECS`.
const POLL_SLEEP_SLICE_MS: u64 = 100;

#[derive(Deserialize)]
struct BrokerPayload {
    code: String,
    iss: String,
}

/// Outcome of the polling loop. Sent exactly once on the channel, then
/// the thread exits.
#[derive(Debug)]
pub enum PollOutcome {
    /// User completed consent on their phone; broker delivered the code
    /// and issuer. Hand these to `VitaOAuthClient::complete_flow`.
    Ready { code: String, iss: String },
    /// Wall-clock timeout (user took too long, or never consented).
    Timeout,
    /// Network/TLS error talking to the broker. Includes the cause.
    Failed(String),
}

/// Handle to a running broker-poll thread.
///
/// Drain [`rx`](Self::rx) once per frame for the [`PollOutcome`]. Call
/// [`cancel`](Self::cancel) to stop polling (e.g. the user backed out of
/// the QR screen); the [`Drop`] impl also cancels, so simply dropping the
/// handle is sufficient to tear the thread down.
pub struct BrokerPoll {
    /// Receiver the caller drains each frame. The poll thread sends at
    /// most one terminal [`PollOutcome`] here; on cancellation nothing is
    /// sent (the thread just exits).
    pub rx: Receiver<PollOutcome>,
    /// Shared cancel flag. Set to `true` by [`cancel`](Self::cancel) /
    /// [`Drop`]; the poll thread checks it before every HTTP request and
    /// between sleep slices, exiting promptly without sending.
    cancel: Arc<AtomicBool>,
    /// Join handle for the poll thread. Held so the thread isn't detached
    /// at construction; in practice it's left to exit on its own (we only
    /// set the cancel flag — see `Drop`), so this is never `join`ed on the
    /// caller's (render) thread, which must never block.
    #[allow(dead_code)]
    join: Option<JoinHandle<()>>,
}

impl BrokerPoll {
    /// Signal the poll thread to stop. The thread observes the flag within
    /// ~`POLL_SLEEP_SLICE_MS` (≈100 ms) when waiting between polls and exits
    /// without sending on `rx`. Idempotent and safe to call from any frame.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for BrokerPoll {
    fn drop(&mut self) {
        // Dropping the handle cancels: set the flag so a still-running poll
        // thread exits on its own (within ~one sleep slice, or when an
        // in-flight 15 s-timeout request returns). We deliberately do NOT
        // `join` here — this Drop runs on the Vita's render thread, which
        // must never block; the thread is harmless once the flag is set
        // (it sends nothing further) and tears itself down in the background.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Spawn a background thread that polls `BROKER_POP_URL` with the given
/// `state` value until a payload arrives, the deadline expires, or a
/// fatal network error occurs. Returns a [`BrokerPoll`] handle: drain its
/// `rx` once per frame for the [`PollOutcome`].
///
/// The thread holds no `AuthClient` and makes only one kind of HTTP
/// request: `GET {BROKER_POP_URL}?state={state}`.
///
/// Cancellation is explicit (not implicit via dropping the receiver):
/// call [`BrokerPoll::cancel`], or drop the whole [`BrokerPoll`] handle
/// (its `Drop` sets the same flag; it does not join, so dropping never
/// blocks the render thread — the poll thread tears itself down in the
/// background). The poll loop checks the flag before every HTTP request and
/// between short sleep slices, so a cancel is honored within
/// ~`POLL_SLEEP_SLICE_MS` (≈100 ms) rather than only when the loop next
/// happens to send. On cancel the thread exits silently — it sends nothing
/// further on `rx`.
pub fn spawn_broker_poll(state: String) -> BrokerPoll {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let join = thread::Builder::new()
        .name("bsky-oauth-broker-poll".into())
        .spawn(move || {
            let cancel = thread_cancel;
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout(Duration::from_secs(15))
                .build();
            let url = format!("{BROKER_POP_URL}?state={state}");
            bsky_log::log!("oauth: poll thread start, state={state}, url={url}");
            let deadline =
                std::time::Instant::now() + Duration::from_secs(POLL_DEADLINE_SECS);
            let mut iter = 0u32;

            loop {
                // Re-check cancellation at the top of every deadline loop.
                if cancel.load(Ordering::Relaxed) {
                    bsky_log::log!("oauth: poll cancelled after {iter} iters");
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    bsky_log::log!("oauth: poll timed out after {iter} iters");
                    let _ = tx.send(PollOutcome::Timeout);
                    return;
                }
                iter += 1;
                // Check again immediately before the (blocking) HTTP request
                // so a cancel right after a sleep slice doesn't fire one more
                // pointless poll at the broker.
                if cancel.load(Ordering::Relaxed) {
                    bsky_log::log!("oauth: poll cancelled before request, iter={iter}");
                    return;
                }
                match agent.get(&url).call() {
                    Ok(resp) => {
                        let status = resp.status();
                        let body = match resp.into_string() {
                            Ok(b) => b,
                            Err(e) => {
                                bsky_log::log!("oauth: poll body read failed: {e}");
                                let _ = tx.send(PollOutcome::Failed(format!("body: {e}")));
                                return;
                            }
                        };
                        bsky_log::log!(
                            "oauth: poll iter={iter} status={status} body_len={}",
                            body.len()
                        );
                        match serde_json::from_str::<BrokerPayload>(&body) {
                            Ok(p) => {
                                bsky_log::log!("oauth: poll got code+iss, sending Ready");
                                let _ = tx.send(PollOutcome::Ready {
                                    code: p.code,
                                    iss: p.iss,
                                });
                                return;
                            }
                            Err(e) => {
                                bsky_log::log!("oauth: poll body not JSON: {e}");
                                let _ = tx.send(PollOutcome::Failed(format!(
                                    "broker returned non-JSON body: {e}"
                                )));
                                return;
                            }
                        }
                    }
                    // 404 = "no entry yet, try again". Anything else is fatal.
                    Err(ureq::Error::Status(404, _)) => {
                        if iter % 5 == 1 {
                            bsky_log::log!("oauth: poll iter={iter} 404 (waiting)");
                        }
                        // Sleep the poll interval in short slices, checking the
                        // cancel flag between each, so a dropped/cancelled
                        // handle is honored within ~POLL_SLEEP_SLICE_MS instead
                        // of up to the full POLL_INTERVAL_SECS.
                        if cancellable_sleep(&cancel) {
                            bsky_log::log!("oauth: poll cancelled during wait, iter={iter}");
                            return;
                        }
                        continue;
                    }
                    Err(ureq::Error::Status(code, _)) => {
                        bsky_log::log!("oauth: poll iter={iter} unexpected HTTP {code}");
                        let _ = tx.send(PollOutcome::Failed(format!(
                            "broker HTTP {code}"
                        )));
                        return;
                    }
                    Err(e) => {
                        bsky_log::log!("oauth: poll iter={iter} transport err: {e}");
                        let _ = tx.send(PollOutcome::Failed(format!("transport: {e}")));
                        return;
                    }
                }
            }
        })
        .expect("spawn bsky-oauth-broker-poll thread");
    BrokerPoll {
        rx,
        cancel,
        join: Some(join),
    }
}

/// Sleep `POLL_INTERVAL_SECS` in `POLL_SLEEP_SLICE_MS` slices, checking the
/// cancel flag between each slice. Returns `true` if cancellation was
/// observed (caller should exit), `false` if the full interval elapsed.
fn cancellable_sleep(cancel: &AtomicBool) -> bool {
    let slices = (POLL_INTERVAL_SECS * 1000) / POLL_SLEEP_SLICE_MS;
    for _ in 0..slices {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        thread::sleep(Duration::from_millis(POLL_SLEEP_SLICE_MS));
    }
    cancel.load(Ordering::Relaxed)
}

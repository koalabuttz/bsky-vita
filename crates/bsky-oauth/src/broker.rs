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

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use serde::Deserialize;

use crate::BROKER_POP_URL;

/// Total wall-clock time to wait for the user to consent on their phone.
/// Bluesky's authorize page session also expires (~5 min); matching here.
const POLL_DEADLINE_SECS: u64 = 300;

/// Sleep between successive `/pop` polls. Keeps below KV eventual-
/// consistency latency (~few seconds) without hammering the Worker.
const POLL_INTERVAL_SECS: u64 = 2;

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

/// Spawn a background thread that polls `BROKER_POP_URL` with the given
/// `state` value until a payload arrives, the deadline expires, or a
/// fatal network error occurs. Returns a receiver the caller drains
/// once per frame.
///
/// The thread holds no `AuthClient` and makes only one kind of HTTP
/// request: `GET {BROKER_POP_URL}?state={state}`. It is cancellable
/// implicitly by dropping the `Receiver` (the thread's `tx.send` will
/// fail and the thread will exit on its next loop iteration).
pub fn spawn_broker_poll(state: String) -> Receiver<PollOutcome> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("bsky-oauth-broker-poll".into())
        .spawn(move || {
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
                if std::time::Instant::now() >= deadline {
                    bsky_log::log!("oauth: poll timed out after {iter} iters");
                    let _ = tx.send(PollOutcome::Timeout);
                    return;
                }
                iter += 1;
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
                        thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));
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
    rx
}

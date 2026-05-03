//! Worker thread for non-blocking PDS calls.
//!
//! Owns an `Arc<AuthClient>` and a thread that runs:
//!
//! ```text
//!   loop { req = recv(); resp = block_on(handle(req)); send(resp); }
//! ```
//!
//! Screens dispatch typed [`WorkRequest`]s via [`Worker::send`]; the render
//! loop drains [`Worker::try_recv`] each frame and hands [`WorkResponse`]s
//! back to the active screen via `Screen::handle_worker_response`.
//!
//! A single worker thread is sufficient for Phase 3:
//! - atrium serializes session-refresh state via `tokio::sync::Mutex`, so
//!   parallel agent calls would block each other anyway.
//! - Timeline fetch + image fetches are sequential from the user's
//!   perspective.
//!
//! ## Lifetime
//!
//! The worker thread runs for as long as the `Worker` struct lives. On
//! drop, the request `Sender` closes; the recv loop returns `Err`, and the
//! thread exits. The `JoinHandle` is held only to suppress the
//! "detached thread" warning — we never join it (the OS will tear down on
//! process exit).
//!
//! ## Send/Sync
//!
//! `Arc<AuthClient>` requires `AuthClient: Send + Sync`. atrium's
//! `AtpAgent` is internally `Send + Sync` (uses `tokio::sync::Mutex`);
//! `ResolvedIdentity` is plain data. If a future atrium revision breaks
//! these bounds, the fallback documented in the Phase 3 plan is to narrow
//! the shared surface to `Arc<dyn HttpClient>` and reconstruct the agent
//! per-screen.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use atrium_api::app::bsky::actor::defs::ProfileViewDetailedData;
use atrium_api::app::bsky::actor::get_profile;
use bsky_auth::AuthClient;
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use futures::executor::block_on;

/// Work the worker thread can be asked to perform. Add a variant per new
/// async operation; keep variants narrow (one network call each) so the
/// response side stays tractable.
pub enum WorkRequest {
    /// Fetch the logged-in user's own profile. The DID is resolved from
    /// the session inside the worker — callers don't need to know it.
    GetOwnProfile,
    // Phase 3.2 will add: FetchTimeline { cursor: Option<String> }
    // Phase 3.5 will add: FetchImage { url: String }
}

/// A completed work item. Each variant's payload mirrors the request that
/// produced it, with a `Result` because every PDS call can fail.
pub enum WorkResponse {
    Profile(Result<Box<ProfileViewDetailedData>, String>),
}

/// Handle to the worker thread. Holds the channel ends and the thread's
/// `JoinHandle`. `send` is non-blocking (unbounded channel); `try_recv`
/// returns `None` if no response is ready yet.
pub struct Worker {
    tx: Sender<WorkRequest>,
    rx: Receiver<WorkResponse>,
    _handle: JoinHandle<()>,
}

impl Worker {
    /// Spawn the worker thread. Takes an `Arc<AuthClient>` so multiple
    /// owners (e.g. a future re-auth screen) can hold a clone, but for
    /// 3.1 only the worker thread holds a clone.
    pub fn spawn(client: Arc<AuthClient>) -> Self {
        let (req_tx, req_rx) = unbounded::<WorkRequest>();
        let (resp_tx, resp_rx) = unbounded::<WorkResponse>();

        let handle = thread::Builder::new()
            .name("bsky-worker".into())
            .spawn(move || run(client, req_rx, resp_tx))
            .expect("spawn bsky-worker thread");

        Self {
            tx: req_tx,
            rx: resp_rx,
            _handle: handle,
        }
    }

    /// Queue a request for the worker. Non-blocking. The response will
    /// arrive on a future call to `try_recv` (typically next frame, but
    /// the worker can take many seconds for a network call).
    pub fn send(&self, req: WorkRequest) {
        // The worker thread only exits when *we* drop. If `send` errors,
        // the channel's other end is gone, which means the worker is
        // already shutting down — silently drop.
        let _ = self.tx.send(req);
    }

    /// Pull the next ready response, or `None` if the worker hasn't
    /// finished any work since the last call. The render loop calls this
    /// in a loop each frame to drain the queue.
    pub fn try_recv(&self) -> Option<WorkResponse> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

fn run(
    client: Arc<AuthClient>,
    requests: Receiver<WorkRequest>,
    responses: Sender<WorkResponse>,
) {
    while let Ok(req) = requests.recv() {
        let resp = handle_request(&client, req);
        if responses.send(resp).is_err() {
            // Main thread dropped the receiver — exit.
            return;
        }
    }
    // Sender dropped on the request side — Worker handle was dropped.
}

fn handle_request(client: &AuthClient, req: WorkRequest) -> WorkResponse {
    match req {
        WorkRequest::GetOwnProfile => {
            let did = match block_on(client.agent.did()) {
                Some(d) => d,
                None => {
                    return WorkResponse::Profile(Err(
                        "agent has no session DID — not logged in".into(),
                    ));
                }
            };
            let result = block_on(client.agent.api.app.bsky.actor.get_profile(
                get_profile::ParametersData {
                    actor: did.into(),
                }
                .into(),
            ));
            match result {
                Ok(p) => WorkResponse::Profile(Ok(Box::new(p.data))),
                Err(e) => WorkResponse::Profile(Err(format!("{e}"))),
            }
        }
    }
}

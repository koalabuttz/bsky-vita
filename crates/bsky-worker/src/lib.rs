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

use std::str::FromStr;

use atrium_api::app::bsky::actor::defs::ProfileViewDetailedData;
use atrium_api::app::bsky::actor::get_profile;
use atrium_api::app::bsky::feed::defs::{
    FeedViewPost, PostView, ThreadViewPostParentRefs, ThreadViewPostRepliesItem,
};
use atrium_api::app::bsky::feed::get_post_thread;
use atrium_api::app::bsky::feed::get_post_thread::OutputThreadRefs;
use atrium_api::app::bsky::feed::get_timeline;
use atrium_api::app::bsky::feed::like::RecordData as LikeRecordData;
use atrium_api::app::bsky::feed::post::{RecordData as PostRecordData, ReplyRefData};
use atrium_api::app::bsky::feed::repost::RecordData as RepostRecordData;
use atrium_api::app::bsky::graph::follow::RecordData as FollowRecordData;
use atrium_api::com::atproto::repo::{create_record, delete_record};
use atrium_api::types::string::{AtIdentifier, Datetime, Did, Nsid, RecordKey};
use atrium_api::types::{LimitedNonZeroU8, LimitedU16, Union, Unknown};
use atrium_xrpc::http::Request;
use atrium_xrpc::HttpClient;
use bsky_auth::AuthClient;
use bsky_net::VitaHttpClient;
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use futures::executor::block_on;

/// Work the worker thread can be asked to perform. Add a variant per new
/// async operation; keep variants narrow (one network call each) so the
/// response side stays tractable.
pub enum WorkRequest {
    /// Fetch a profile. `actor: None` means the logged-in user's own
    /// profile (DID resolved from the session). `Some(handle_or_did)`
    /// fetches that actor's profile.
    FetchProfile { actor: Option<String> },
    /// Fetch a page of the home (Following) timeline. `cursor: None` for
    /// the first page; subsequent pages pass the cursor returned in the
    /// previous `TimelineBatch`. End-of-feed is signalled by a response
    /// with `cursor: None`.
    FetchTimeline { cursor: Option<String> },
    /// Fetch an arbitrary image URL (avatars, embeds). No auth: uses a
    /// fresh `VitaHttpClient` directly, bypassing the agent's session
    /// machinery. The URL is echoed back in the response so the main
    /// thread can dispatch the result to the right cache key.
    FetchImage { url: String },
    /// Create a new top-level post or reply via
    /// `com.atproto.repo.createRecord` with collection
    /// `app.bsky.feed.post`. `reply_to: None` ⇒ top-level; `Some(_)` ⇒
    /// reply (parent + root strong refs).
    CreatePost {
        text: String,
        reply_to: Option<ReplyTarget>,
    },
    /// Create a like record (collection `app.bsky.feed.like`) for the
    /// given post. Caller updates UI optimistically; the worker
    /// confirms with the new record's URI.
    CreateLike { post_uri: String, post_cid: String },
    /// Delete a like by record-key (extracted from the like's AT-URI).
    DeleteLike { rkey: String },
    /// Create a repost record for the given post.
    CreateRepost { post_uri: String, post_cid: String },
    /// Delete a repost by record-key.
    DeleteRepost { rkey: String },
    /// Fetch a post's thread via `app.bsky.feed.getPostThread`. The
    /// `uri` is the AT-URI of any post in the thread; the response
    /// includes parent ancestors (oldest-first), the main post, and
    /// direct replies.
    FetchThread { uri: String },
    /// Follow an actor (collection `app.bsky.graph.follow`). Subject
    /// is the target actor's DID.
    CreateFollow { actor_did: String },
    /// Unfollow by record-key (extracted from the follow's AT-URI).
    DeleteFollow { rkey: String },
}

/// Minimal data the caller provides for a reply. The worker translates
/// these strings into the typed `ReplyRefData` atrium expects. For
/// thread replies, `root_*` should be the thread's actual root (read
/// from the parent post's `record.reply.root` if present); for replies
/// to a top-level post, parent and root are the same. Phase 4.2 MVP:
/// callers simply pass parent for both — replies within a thread may
/// render at the wrong place in some clients until 4.4 reads thread
/// context.
#[derive(Clone, Debug)]
pub struct ReplyTarget {
    pub parent_uri: String,
    pub parent_cid: String,
    pub root_uri: String,
    pub root_cid: String,
}

/// One page of timeline posts plus the cursor for the next page. `cursor:
/// None` means we've reached the end of the feed.
pub struct TimelineBatch {
    pub posts: Vec<FeedViewPost>,
    pub cursor: Option<String>,
}

/// A flattened thread view — ancestors above the focus (oldest first),
/// the focused post itself, and direct replies. Phase 4.4 MVP: replies
/// are first-level only (no nested reply rendering); future phases
/// can recurse.
pub struct ThreadBatch {
    /// Ancestors of `main`, in oldest → newest order. The first entry
    /// is the thread's root; the last entry is the parent of `main`.
    /// Empty if `main` is itself the root.
    pub parents: Vec<PostView>,
    /// The post the user tapped on (the "focus" of the thread).
    pub main: PostView,
    /// Direct replies to `main`.
    pub replies: Vec<PostView>,
}

/// A completed work item. Each variant's payload mirrors the request that
/// produced it, with a `Result` because every PDS call can fail.
pub enum WorkResponse {
    Profile(Result<Box<ProfileViewDetailedData>, String>),
    Timeline(Result<TimelineBatch, String>),
    /// Raw bytes of the requested image. Caller decodes with
    /// `bsky_render::Texture::from_image_bytes`. `url` echoes the
    /// request URL so callers can route the result to the right cache
    /// entry / clear the right "in-flight" tracker.
    Image {
        url: String,
        bytes: Result<Vec<u8>, String>,
    },
    /// AT-URI of the just-created post on success; error string on
    /// failure (lexicon validation, network, auth, etc.).
    PostCreated(Result<String, String>),
    /// `Ok(Some(uri))` for CreateLike; `Ok(None)` for DeleteLike;
    /// `Err` for either.
    LikeChanged(Result<Option<String>, String>),
    /// Same shape as `LikeChanged`, for repost create/delete.
    RepostChanged(Result<Option<String>, String>),
    /// `Ok(Some(uri))` for CreateFollow; `Ok(None)` for DeleteFollow;
    /// `Err` for either.
    FollowChanged(Result<Option<String>, String>),
    Thread(Result<ThreadBatch, String>),
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
        WorkRequest::FetchProfile { actor } => {
            // Resolve actor: None ⇒ session's own DID; Some(s) ⇒ use directly.
            let actor_str = match actor {
                Some(a) => a,
                None => match block_on(client.agent.did()) {
                    Some(d) => d.to_string(),
                    None => {
                        return WorkResponse::Profile(Err(
                            "agent has no session DID — not logged in".into(),
                        ));
                    }
                },
            };
            let at_id = match AtIdentifier::from_str(&actor_str) {
                Ok(id) => id,
                Err(e) => {
                    return WorkResponse::Profile(Err(format!(
                        "invalid actor identifier {actor_str:?}: {e}"
                    )));
                }
            };
            let result = block_on(client.agent.api.app.bsky.actor.get_profile(
                get_profile::ParametersData { actor: at_id }.into(),
            ));
            match result {
                Ok(p) => WorkResponse::Profile(Ok(Box::new(p.data))),
                Err(e) => WorkResponse::Profile(Err(format!("{e}"))),
            }
        }
        WorkRequest::FetchTimeline { cursor } => {
            // Page size 50: covers ~10 screens of posts on a Vita display
            // and is well under atrium's cap of 100.
            let limit = LimitedNonZeroU8::<100>::try_from(50)
                .expect("50 fits in LimitedNonZeroU8<100>");
            let params = get_timeline::ParametersData {
                cursor,
                limit: Some(limit),
                algorithm: None,
            }
            .into();
            match block_on(client.agent.api.app.bsky.feed.get_timeline(params)) {
                Ok(o) => WorkResponse::Timeline(Ok(TimelineBatch {
                    posts: o.data.feed,
                    cursor: o.data.cursor,
                })),
                Err(e) => WorkResponse::Timeline(Err(format!("{e}"))),
            }
        }
        WorkRequest::FetchImage { url } => {
            let bytes = fetch_image_bytes(&url);
            WorkResponse::Image { url, bytes }
        }
        WorkRequest::CreatePost { text, reply_to } => create_post(client, text, reply_to),
        WorkRequest::CreateLike {
            post_uri,
            post_cid,
        } => create_engagement_record(
            client,
            "app.bsky.feed.like",
            &post_uri,
            &post_cid,
            true,
        ),
        WorkRequest::DeleteLike { rkey } => {
            delete_engagement_record(client, "app.bsky.feed.like", &rkey, true)
        }
        WorkRequest::CreateRepost {
            post_uri,
            post_cid,
        } => create_engagement_record(
            client,
            "app.bsky.feed.repost",
            &post_uri,
            &post_cid,
            false,
        ),
        WorkRequest::DeleteRepost { rkey } => {
            delete_engagement_record(client, "app.bsky.feed.repost", &rkey, false)
        }
        WorkRequest::FetchThread { uri } => fetch_thread(client, uri),
        WorkRequest::CreateFollow { actor_did } => create_follow(client, actor_did),
        WorkRequest::DeleteFollow { rkey } => delete_follow(client, &rkey),
    }
}

/// Create an `app.bsky.graph.follow` record targeting `actor_did`.
fn create_follow(client: &AuthClient, actor_did: String) -> WorkResponse {
    let did_str = match block_on(client.agent.did()) {
        Some(d) => d.to_string(),
        None => return WorkResponse::FollowChanged(Err("no session DID".into())),
    };
    let repo = match AtIdentifier::from_str(&did_str) {
        Ok(id) => id,
        Err(e) => return WorkResponse::FollowChanged(Err(format!("DID parse: {e}"))),
    };
    let subject = match Did::from_str(&actor_did) {
        Ok(d) => d,
        Err(e) => return WorkResponse::FollowChanged(Err(format!("actor DID parse: {e}"))),
    };
    let mut json = match serde_json::to_value(FollowRecordData {
        created_at: Datetime::now(),
        subject,
    }) {
        Ok(v) => v,
        Err(e) => return WorkResponse::FollowChanged(Err(format!("serialize: {e}"))),
    };
    if let serde_json::Value::Object(map) = &mut json {
        map.insert(
            "$type".to_string(),
            serde_json::Value::String("app.bsky.graph.follow".to_string()),
        );
    }
    let unknown: Unknown = match serde_json::from_value(json) {
        Ok(u) => u,
        Err(e) => return WorkResponse::FollowChanged(Err(format!("re-deserialize: {e}"))),
    };
    let collection = match Nsid::from_str("app.bsky.graph.follow") {
        Ok(n) => n,
        Err(e) => return WorkResponse::FollowChanged(Err(format!("nsid: {e}"))),
    };
    let input = create_record::InputData {
        collection,
        record: unknown,
        repo,
        rkey: None,
        swap_commit: None,
        validate: None,
    };
    match block_on(client.agent.api.com.atproto.repo.create_record(input.into())) {
        Ok(o) => WorkResponse::FollowChanged(Ok(Some(o.data.uri))),
        Err(e) => WorkResponse::FollowChanged(Err(format!("{e}"))),
    }
}

fn delete_follow(client: &AuthClient, rkey_str: &str) -> WorkResponse {
    let did_str = match block_on(client.agent.did()) {
        Some(d) => d.to_string(),
        None => return WorkResponse::FollowChanged(Err("no session DID".into())),
    };
    let repo = match AtIdentifier::from_str(&did_str) {
        Ok(id) => id,
        Err(e) => return WorkResponse::FollowChanged(Err(format!("DID parse: {e}"))),
    };
    let collection = match Nsid::from_str("app.bsky.graph.follow") {
        Ok(n) => n,
        Err(e) => return WorkResponse::FollowChanged(Err(format!("nsid: {e}"))),
    };
    let rkey = match RecordKey::from_str(rkey_str) {
        Ok(k) => k,
        Err(e) => return WorkResponse::FollowChanged(Err(format!("rkey: {e}"))),
    };
    let input = delete_record::InputData {
        collection,
        repo,
        rkey,
        swap_commit: None,
        swap_record: None,
    };
    match block_on(client.agent.api.com.atproto.repo.delete_record(input.into())) {
        Ok(_) => WorkResponse::FollowChanged(Ok(None)),
        Err(e) => WorkResponse::FollowChanged(Err(format!("{e}"))),
    }
}

/// Fetch a post's thread + flatten into a `ThreadBatch`.
fn fetch_thread(client: &AuthClient, uri: String) -> WorkResponse {
    let params = get_post_thread::ParametersData {
        depth: LimitedU16::<1000>::try_from(2).ok(),
        parent_height: LimitedU16::<1000>::try_from(10).ok(),
        uri,
    }
    .into();
    match block_on(client.agent.api.app.bsky.feed.get_post_thread(params)) {
        Ok(o) => match o.data.thread {
            Union::Refs(OutputThreadRefs::AppBskyFeedDefsThreadViewPost(view)) => {
                let main = view.post.clone();
                let mut parents = Vec::new();
                walk_parents(&view, &mut parents);
                parents.reverse();
                let replies: Vec<PostView> = view
                    .replies
                    .as_ref()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| match item {
                                Union::Refs(ThreadViewPostRepliesItem::ThreadViewPost(
                                    child,
                                )) => Some(child.post.clone()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                WorkResponse::Thread(Ok(ThreadBatch {
                    parents,
                    main,
                    replies,
                }))
            }
            Union::Refs(OutputThreadRefs::AppBskyFeedDefsNotFoundPost(_)) => {
                WorkResponse::Thread(Err("post not found".into()))
            }
            Union::Refs(OutputThreadRefs::AppBskyFeedDefsBlockedPost(_)) => {
                WorkResponse::Thread(Err("post is blocked".into()))
            }
            Union::Unknown(_) => {
                WorkResponse::Thread(Err("unsupported thread variant".into()))
            }
        },
        Err(e) => WorkResponse::Thread(Err(format!("{e}"))),
    }
}

/// Walk the parent chain of a `ThreadViewPost` and push each ancestor
/// into `out` (newest-first; caller reverses for oldest-first).
fn walk_parents(
    view: &atrium_api::app::bsky::feed::defs::ThreadViewPost,
    out: &mut Vec<PostView>,
) {
    if let Some(Union::Refs(ThreadViewPostParentRefs::ThreadViewPost(parent))) =
        view.parent.as_ref()
    {
        out.push(parent.post.clone());
        walk_parents(parent, out);
    }
}

/// Build + submit a like or repost record. `is_like` selects which
/// record type + which response variant to wrap the result in.
fn create_engagement_record(
    client: &AuthClient,
    collection_str: &str,
    post_uri: &str,
    post_cid: &str,
    is_like: bool,
) -> WorkResponse {
    use atrium_api::com::atproto::repo::strong_ref::MainData as StrongRefData;
    use atrium_api::types::string::Cid;

    let did_str = match block_on(client.agent.did()) {
        Some(d) => d.to_string(),
        None => return wrap_engagement_err(is_like, "no session DID".into()),
    };
    let repo = match AtIdentifier::from_str(&did_str) {
        Ok(id) => id,
        Err(e) => return wrap_engagement_err(is_like, format!("DID parse: {e}")),
    };
    let cid = match Cid::from_str(post_cid) {
        Ok(c) => c,
        Err(e) => return wrap_engagement_err(is_like, format!("cid parse: {e}")),
    };
    let subject = StrongRefData {
        cid,
        uri: post_uri.to_string(),
    }
    .into();
    let collection = match Nsid::from_str(collection_str) {
        Ok(n) => n,
        Err(e) => return wrap_engagement_err(is_like, format!("nsid: {e}")),
    };

    // Serialize the appropriate record type → JSON → inject $type → Unknown.
    let mut json = if is_like {
        match serde_json::to_value(LikeRecordData {
            created_at: Datetime::now(),
            subject,
            via: None,
        }) {
            Ok(v) => v,
            Err(e) => return wrap_engagement_err(is_like, format!("serialize: {e}")),
        }
    } else {
        match serde_json::to_value(RepostRecordData {
            created_at: Datetime::now(),
            subject,
            via: None,
        }) {
            Ok(v) => v,
            Err(e) => return wrap_engagement_err(is_like, format!("serialize: {e}")),
        }
    };
    if let serde_json::Value::Object(map) = &mut json {
        map.insert(
            "$type".to_string(),
            serde_json::Value::String(collection_str.to_string()),
        );
    }
    let unknown: Unknown = match serde_json::from_value(json) {
        Ok(u) => u,
        Err(e) => return wrap_engagement_err(is_like, format!("re-deserialize: {e}")),
    };

    let input = create_record::InputData {
        collection,
        record: unknown,
        repo,
        rkey: None,
        swap_commit: None,
        validate: None,
    };
    match block_on(client.agent.api.com.atproto.repo.create_record(input.into())) {
        Ok(o) => wrap_engagement_ok(is_like, Some(o.data.uri)),
        Err(e) => wrap_engagement_err(is_like, format!("{e}")),
    }
}

/// Delete a like or repost record by rkey.
fn delete_engagement_record(
    client: &AuthClient,
    collection_str: &str,
    rkey_str: &str,
    is_like: bool,
) -> WorkResponse {
    let did_str = match block_on(client.agent.did()) {
        Some(d) => d.to_string(),
        None => return wrap_engagement_err(is_like, "no session DID".into()),
    };
    let repo = match AtIdentifier::from_str(&did_str) {
        Ok(id) => id,
        Err(e) => return wrap_engagement_err(is_like, format!("DID parse: {e}")),
    };
    let collection = match Nsid::from_str(collection_str) {
        Ok(n) => n,
        Err(e) => return wrap_engagement_err(is_like, format!("nsid: {e}")),
    };
    let rkey = match RecordKey::from_str(rkey_str) {
        Ok(k) => k,
        Err(e) => return wrap_engagement_err(is_like, format!("rkey: {e}")),
    };
    let input = delete_record::InputData {
        collection,
        repo,
        rkey,
        swap_commit: None,
        swap_record: None,
    };
    match block_on(client.agent.api.com.atproto.repo.delete_record(input.into())) {
        Ok(_) => wrap_engagement_ok(is_like, None),
        Err(e) => wrap_engagement_err(is_like, format!("{e}")),
    }
}

fn wrap_engagement_ok(is_like: bool, uri: Option<String>) -> WorkResponse {
    if is_like {
        WorkResponse::LikeChanged(Ok(uri))
    } else {
        WorkResponse::RepostChanged(Ok(uri))
    }
}

fn wrap_engagement_err(is_like: bool, msg: String) -> WorkResponse {
    if is_like {
        WorkResponse::LikeChanged(Err(msg))
    } else {
        WorkResponse::RepostChanged(Err(msg))
    }
}

/// Build + submit a new `app.bsky.feed.post` record. Returns the
/// AT-URI of the created post on success.
fn create_post(
    client: &AuthClient,
    text: String,
    reply_to: Option<ReplyTarget>,
) -> WorkResponse {
    use atrium_api::com::atproto::repo::strong_ref::MainData as StrongRefData;
    use atrium_api::types::string::Cid;

    let did_str = match block_on(client.agent.did()) {
        Some(d) => d.to_string(),
        None => {
            return WorkResponse::PostCreated(Err(
                "agent has no session DID — not logged in".into(),
            ));
        }
    };
    let repo = match AtIdentifier::from_str(&did_str) {
        Ok(id) => id,
        Err(e) => {
            return WorkResponse::PostCreated(Err(format!(
                "DID parse failed for {did_str:?}: {e}"
            )));
        }
    };

    // Translate ReplyTarget → atrium ReplyRefData if present.
    let reply = match reply_to {
        Some(rt) => {
            let parent_cid = match Cid::from_str(&rt.parent_cid) {
                Ok(c) => c,
                Err(e) => {
                    return WorkResponse::PostCreated(Err(format!(
                        "parent cid parse: {e}"
                    )));
                }
            };
            let root_cid = match Cid::from_str(&rt.root_cid) {
                Ok(c) => c,
                Err(e) => {
                    return WorkResponse::PostCreated(Err(format!("root cid parse: {e}")));
                }
            };
            Some(
                ReplyRefData {
                    parent: StrongRefData {
                        cid: parent_cid,
                        uri: rt.parent_uri,
                    }
                    .into(),
                    root: StrongRefData {
                        cid: root_cid,
                        uri: rt.root_uri,
                    }
                    .into(),
                }
                .into(),
            )
        }
        None => None,
    };

    let record = PostRecordData {
        text,
        created_at: Datetime::now(),
        reply,
        embed: None,
        entities: None,
        facets: None,
        labels: None,
        langs: None,
        tags: None,
    };

    // atrium's typed RecordData doesn't carry `$type` — Bluesky's
    // server requires it on createRecord. Round-trip through serde_json
    // to inject it before converting to the wire-shape `Unknown`.
    let mut json = match serde_json::to_value(&record) {
        Ok(v) => v,
        Err(e) => return WorkResponse::PostCreated(Err(format!("serialize: {e}"))),
    };
    if let serde_json::Value::Object(map) = &mut json {
        map.insert(
            "$type".to_string(),
            serde_json::Value::String("app.bsky.feed.post".to_string()),
        );
    } else {
        return WorkResponse::PostCreated(Err(
            "post record didn't serialize as a JSON object".into(),
        ));
    }
    let unknown: Unknown = match serde_json::from_value(json) {
        Ok(u) => u,
        Err(e) => return WorkResponse::PostCreated(Err(format!("re-deserialize: {e}"))),
    };

    let collection = match Nsid::from_str("app.bsky.feed.post") {
        Ok(n) => n,
        Err(e) => return WorkResponse::PostCreated(Err(format!("nsid parse: {e}"))),
    };
    let input = create_record::InputData {
        collection,
        record: unknown,
        repo,
        rkey: None,
        swap_commit: None,
        validate: None,
    };
    match block_on(client.agent.api.com.atproto.repo.create_record(input.into())) {
        Ok(o) => WorkResponse::PostCreated(Ok(o.data.uri)),
        Err(e) => WorkResponse::PostCreated(Err(format!("{e}"))),
    }
}

/// GET `url`, return the response body as bytes. Uses a fresh
/// VitaHttpClient (no auth) — the CDN endpoints (cdn.bsky.app) don't
/// require Bearer tokens and atrium's session-refresh path would only
/// add overhead.
fn fetch_image_bytes(url: &str) -> Result<Vec<u8>, String> {
    let http = VitaHttpClient::new();
    let req = match Request::builder()
        .method("GET")
        .uri(url)
        .body(Vec::<u8>::new())
    {
        Ok(r) => r,
        Err(e) => return Err(format!("invalid request URI: {e}")),
    };
    match block_on(http.send_http(req)) {
        Ok(resp) if resp.status().is_success() => Ok(resp.into_body()),
        Ok(resp) => Err(format!("HTTP {}", resp.status().as_u16())),
        Err(e) => Err(format!("{e}")),
    }
}

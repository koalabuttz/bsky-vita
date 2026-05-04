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

use atrium_api::app::bsky::actor::defs::{
    PreferencesItem, ProfileView, ProfileViewDetailedData,
};
use atrium_api::app::bsky::actor::{get_preferences, get_profile, search_actors};
use atrium_api::app::bsky::feed::search_posts;
use atrium_api::app::bsky::feed::defs::{
    FeedViewPost, PostView, ThreadViewPostParentRefs, ThreadViewPostRepliesItem,
};
use atrium_api::app::bsky::feed::get_feed;
use atrium_api::app::bsky::feed::get_feed_generators;
use atrium_api::app::bsky::feed::get_post_thread;
use atrium_api::app::bsky::feed::get_post_thread::OutputThreadRefs;
use atrium_api::app::bsky::feed::get_timeline;
use atrium_api::app::bsky::feed::like::RecordData as LikeRecordData;
use atrium_api::app::bsky::feed::post::{RecordData as PostRecordData, ReplyRefData};
use atrium_api::app::bsky::feed::repost::RecordData as RepostRecordData;
use atrium_api::app::bsky::graph::follow::RecordData as FollowRecordData;
use atrium_api::app::bsky::notification::list_notifications;
use atrium_api::app::bsky::notification::list_notifications::Notification;
use atrium_api::app::bsky::notification::update_seen;
use atrium_api::com::atproto::repo::{create_record, delete_record};
use atrium_api::types::string::{AtIdentifier, Datetime, Did, Nsid, RecordKey};
use atrium_api::types::{LimitedNonZeroU8, LimitedU16, Union, Unknown};
use atrium_xrpc::http::Request;
use atrium_xrpc::HttpClient;
use bsky_auth::AuthClient;
use bsky_net::VitaHttpClient;
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use futures::executor::block_on;

/// Identifies which feed a `FetchFeed` request targets.
///
/// `Following` uses `app.bsky.feed.getTimeline` (the user's home feed);
/// `Feed(uri)` uses `app.bsky.feed.getFeed` against an arbitrary feed
/// generator's AT-URI (typically read from the user's pinned-feeds
/// preference). The same enum is echoed back in `FeedPage` responses so
/// `TimelineScreen` can drop stale responses for feeds the user has
/// switched away from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FeedSource {
    /// User's home Following timeline.
    Following,
    /// Custom feed by AT-URI (e.g. `at://did:plc:.../app.bsky.feed.generator/whats-hot`).
    /// Phase 5.1 doesn't support `app.bsky.graph.list` URIs (lists);
    /// those use a separate `getListFeed` endpoint and are deferred.
    Feed(String),
}

/// Work the worker thread can be asked to perform. Add a variant per new
/// async operation; keep variants narrow (one network call each) so the
/// response side stays tractable.
pub enum WorkRequest {
    /// Fetch a profile. `actor: None` means the logged-in user's own
    /// profile (DID resolved from the session). `Some(handle_or_did)`
    /// fetches that actor's profile.
    FetchProfile { actor: Option<String> },
    /// Fetch a page of `source`. `cursor: None` for the first page;
    /// subsequent pages pass the cursor returned in the previous
    /// `FeedPage` response. End-of-feed is signalled by a response with
    /// `cursor: None`.
    FetchFeed {
        source: FeedSource,
        cursor: Option<String>,
    },
    /// Fetch the user's pinned-feeds preference + hydrate generator
    /// metadata (display name, avatar) for each pinned custom feed in
    /// one round-trip via `app.bsky.feed.getFeedGenerators`.
    FetchSavedFeeds,
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
    /// Fetch a page of the user's notifications via
    /// `app.bsky.notification.listNotifications`.
    FetchNotifications { cursor: Option<String> },
    /// Mark notifications as seen up to `seen_at`. Fire-and-forget; we
    /// don't surface a response variant since failure is non-fatal
    /// (server tracks read state independently).
    MarkSeen { seen_at: Datetime },
    /// Search for actors matching `q`. Cursor-paginated.
    SearchActors {
        q: String,
        cursor: Option<String>,
    },
    /// Search for posts matching `q`. Cursor-paginated.
    SearchPosts {
        q: String,
        cursor: Option<String>,
    },
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

/// One pinned saved-feed entry, hydrated with display metadata so the
/// pill row can render without an additional fetch.
#[derive(Clone, Debug)]
pub struct SavedFeedPin {
    pub source: FeedSource,
    /// Display name. `"Following"` for the Following pin; the feed
    /// generator's `display_name` for custom feeds (or a fallback
    /// extracted from the AT-URI if hydration failed).
    pub display_name: String,
    /// Avatar URL (from the generator's metadata). `None` for the
    /// Following pin or when hydration failed.
    pub avatar_url: Option<String>,
}

/// User's pinned saved feeds, in display order. Always begins with a
/// `Following` entry (we synthesize it if the user has un-pinned the
/// Following timeline in their prefs — Following is the conceptual home
/// view and we always offer it as the first pill).
pub struct SavedFeedsBatch {
    pub pins: Vec<SavedFeedPin>,
}

/// One page of notifications + the next-page cursor (None = end).
pub struct NotificationBatch {
    pub notifications: Vec<Notification>,
    pub cursor: Option<String>,
}

/// One page of actor search results.
pub struct ActorsBatch {
    pub actors: Vec<ProfileView>,
    pub cursor: Option<String>,
}

/// One page of post search results.
pub struct PostsBatch {
    pub posts: Vec<PostView>,
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
    /// One page of feed posts. `source` echoes the request so screens
    /// holding multiple feeds in flight (or that have switched away
    /// from a feed) can route or drop the response correctly.
    FeedPage {
        source: FeedSource,
        batch: Result<TimelineBatch, String>,
    },
    /// User's pinned saved feeds (already hydrated with display metadata).
    SavedFeeds(Result<SavedFeedsBatch, String>),
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
    Notifications(Result<NotificationBatch, String>),
    SearchActors(Result<ActorsBatch, String>),
    SearchPosts(Result<PostsBatch, String>),
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
        log_if_err(&resp);
        if responses.send(resp).is_err() {
            // Main thread dropped the receiver — exit.
            return;
        }
    }
    // Sender dropped on the request side — Worker handle was dropped.
}

/// Append a one-line summary to the disk log if `resp` carries an
/// `Err`. Lets us inspect post-mortem when a screen swallowed a
/// network failure (most do — they show fallbacks rather than error
/// UIs).
fn log_if_err(resp: &WorkResponse) {
    match resp {
        WorkResponse::Profile(Err(e)) => bsky_log::log!("worker: Profile err: {e}"),
        WorkResponse::FeedPage { source, batch: Err(e) } => {
            bsky_log::log!("worker: FeedPage({source:?}) err: {e}")
        }
        WorkResponse::SavedFeeds(Err(e)) => bsky_log::log!("worker: SavedFeeds err: {e}"),
        WorkResponse::Image { url, bytes: Err(e) } => {
            bsky_log::log!("worker: Image({url}) err: {e}")
        }
        WorkResponse::PostCreated(Err(e)) => bsky_log::log!("worker: PostCreated err: {e}"),
        WorkResponse::LikeChanged(Err(e)) => bsky_log::log!("worker: LikeChanged err: {e}"),
        WorkResponse::RepostChanged(Err(e)) => {
            bsky_log::log!("worker: RepostChanged err: {e}")
        }
        WorkResponse::FollowChanged(Err(e)) => {
            bsky_log::log!("worker: FollowChanged err: {e}")
        }
        WorkResponse::Thread(Err(e)) => bsky_log::log!("worker: Thread err: {e}"),
        WorkResponse::Notifications(Err(e)) => {
            bsky_log::log!("worker: Notifications err: {e}")
        }
        WorkResponse::SearchActors(Err(e)) => {
            bsky_log::log!("worker: SearchActors err: {e}")
        }
        WorkResponse::SearchPosts(Err(e)) => {
            bsky_log::log!("worker: SearchPosts err: {e}")
        }
        _ => {}
    }
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
        WorkRequest::FetchFeed { source, cursor } => fetch_feed_page(client, source, cursor),
        WorkRequest::FetchSavedFeeds => fetch_saved_feeds(client),
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
        WorkRequest::SearchActors { q, cursor } => search_actors_handler(client, q, cursor),
        WorkRequest::SearchPosts { q, cursor } => search_posts_handler(client, q, cursor),
        WorkRequest::FetchNotifications { cursor } => fetch_notifications(client, cursor),
        WorkRequest::MarkSeen { seen_at } => {
            // Fire-and-forget; the result is logged but not surfaced.
            let input = update_seen::InputData { seen_at }.into();
            if let Err(e) =
                block_on(client.agent.api.app.bsky.notification.update_seen(input))
            {
                bsky_log::log!("update_seen failed: {e}");
            }
            // Reuse Notifications response just to fit the WorkResponse
            // shape; the inner Ok with empty batch carries no data.
            WorkResponse::Notifications(Ok(NotificationBatch {
                notifications: Vec::new(),
                cursor: None,
            }))
        }
    }
}

fn search_actors_handler(
    client: &AuthClient,
    q: String,
    cursor: Option<String>,
) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(25).expect("25 fits");
    let params = search_actors::ParametersData {
        cursor,
        limit: Some(limit),
        q: Some(q),
        term: None,
    }
    .into();
    match block_on(client.agent.api.app.bsky.actor.search_actors(params)) {
        Ok(o) => WorkResponse::SearchActors(Ok(ActorsBatch {
            actors: o.data.actors,
            cursor: o.data.cursor,
        })),
        Err(e) => WorkResponse::SearchActors(Err(format!("{e}"))),
    }
}

fn search_posts_handler(
    client: &AuthClient,
    q: String,
    cursor: Option<String>,
) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(25).expect("25 fits");
    let params = search_posts::ParametersData {
        author: None,
        cursor,
        domain: None,
        lang: None,
        limit: Some(limit),
        mentions: None,
        q,
        since: None,
        sort: None,
        tag: None,
        until: None,
        url: None,
    }
    .into();
    match block_on(client.agent.api.app.bsky.feed.search_posts(params)) {
        Ok(o) => WorkResponse::SearchPosts(Ok(PostsBatch {
            posts: o.data.posts,
            cursor: o.data.cursor,
        })),
        Err(e) => WorkResponse::SearchPosts(Err(format!("{e}"))),
    }
}

/// Fetch one page of `source`. Page size 50 (well under atrium's cap of
/// 100). For Following: `getTimeline`; for custom feeds: `getFeed`.
fn fetch_feed_page(
    client: &AuthClient,
    source: FeedSource,
    cursor: Option<String>,
) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(50)
        .expect("50 fits in LimitedNonZeroU8<100>");
    match &source {
        FeedSource::Following => {
            let params = get_timeline::ParametersData {
                cursor,
                limit: Some(limit),
                algorithm: None,
            }
            .into();
            match block_on(client.agent.api.app.bsky.feed.get_timeline(params)) {
                Ok(o) => WorkResponse::FeedPage {
                    source,
                    batch: Ok(TimelineBatch {
                        posts: o.data.feed,
                        cursor: o.data.cursor,
                    }),
                },
                Err(e) => WorkResponse::FeedPage {
                    source,
                    batch: Err(format!("{e}")),
                },
            }
        }
        FeedSource::Feed(uri) => {
            let params = get_feed::ParametersData {
                cursor,
                feed: uri.clone(),
                limit: Some(limit),
            }
            .into();
            match block_on(client.agent.api.app.bsky.feed.get_feed(params)) {
                Ok(o) => WorkResponse::FeedPage {
                    source,
                    batch: Ok(TimelineBatch {
                        posts: o.data.feed,
                        cursor: o.data.cursor,
                    }),
                },
                Err(e) => WorkResponse::FeedPage {
                    source,
                    batch: Err(format!("{e}")),
                },
            }
        }
    }
}

/// Read the user's pinned-feeds preference + hydrate generator metadata
/// in one round-trip. Output always begins with a `Following` pin (we
/// synthesize one if it's not in the prefs).
fn fetch_saved_feeds(client: &AuthClient) -> WorkResponse {
    // 1. Read prefs.
    let params = get_preferences::ParametersData {}.into();
    let prefs = match block_on(client.agent.api.app.bsky.actor.get_preferences(params)) {
        Ok(o) => o.data.preferences,
        Err(e) => return WorkResponse::SavedFeeds(Err(format!("{e}"))),
    };

    // 2. Walk to find a SavedFeedsPrefV2 (preferred) or fall back to v1.
    let mut v2_items: Option<Vec<atrium_api::app::bsky::actor::defs::SavedFeed>> = None;
    let mut v1_pinned: Option<Vec<String>> = None;
    for item in prefs.iter() {
        if let Union::Refs(refs) = item {
            match refs {
                PreferencesItem::SavedFeedsPrefV2(b) => {
                    v2_items = Some(b.data.items.clone());
                    break;
                }
                PreferencesItem::SavedFeedsPref(b) => {
                    v1_pinned = Some(b.data.pinned.clone());
                }
                _ => {}
            }
        }
    }

    // 3. Project into PendingPin: a pin record before display-name hydration.
    enum PendingPin {
        Following,
        Feed { uri: String },
    }
    let mut pending: Vec<PendingPin> = Vec::new();
    let mut have_following = false;
    if let Some(items) = v2_items {
        for it in items.iter() {
            if !it.data.pinned {
                continue;
            }
            match it.data.r#type.as_str() {
                "timeline" => {
                    pending.push(PendingPin::Following);
                    have_following = true;
                }
                "feed" => pending.push(PendingPin::Feed {
                    uri: it.data.value.clone(),
                }),
                // Skip "list" (deferred) + any unknown type.
                _ => {}
            }
        }
    } else if let Some(pinned) = v1_pinned {
        for uri in pinned {
            pending.push(PendingPin::Feed { uri });
        }
    }
    if !have_following {
        // Always show Following as the first pill, even if the user has
        // un-pinned it in their prefs.
        pending.insert(0, PendingPin::Following);
    }

    // 4. Hydrate display names + avatars for all Feed entries in one call.
    let feed_uris: Vec<String> = pending
        .iter()
        .filter_map(|p| match p {
            PendingPin::Feed { uri } => Some(uri.clone()),
            PendingPin::Following => None,
        })
        .collect();
    let mut hydrated: std::collections::HashMap<String, (String, Option<String>)> =
        std::collections::HashMap::new();
    if !feed_uris.is_empty() {
        let params = get_feed_generators::ParametersData { feeds: feed_uris }.into();
        match block_on(client.agent.api.app.bsky.feed.get_feed_generators(params)) {
            Ok(o) => {
                for view in o.data.feeds.iter() {
                    hydrated.insert(
                        view.data.uri.clone(),
                        (view.data.display_name.clone(), view.data.avatar.clone()),
                    );
                }
            }
            Err(e) => {
                bsky_log::log!(
                    "get_feed_generators failed: {e} — pills will use AT-URI fallbacks"
                );
            }
        }
    }

    // 5. Materialize SavedFeedPin list.
    let pins: Vec<SavedFeedPin> = pending
        .into_iter()
        .map(|p| match p {
            PendingPin::Following => SavedFeedPin {
                source: FeedSource::Following,
                display_name: "Following".to_string(),
                avatar_url: None,
            },
            PendingPin::Feed { uri } => {
                let (display_name, avatar_url) =
                    hydrated.get(&uri).cloned().unwrap_or_else(|| {
                        // Fallback: the rkey portion of the AT-URI if hydration
                        // failed. Better than rendering an empty pill.
                        let fallback = uri.rsplit('/').next().unwrap_or(&uri).to_string();
                        (fallback, None)
                    });
                SavedFeedPin {
                    source: FeedSource::Feed(uri),
                    display_name,
                    avatar_url,
                }
            }
        })
        .collect();
    WorkResponse::SavedFeeds(Ok(SavedFeedsBatch { pins }))
}

fn fetch_notifications(client: &AuthClient, cursor: Option<String>) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(50)
        .expect("50 fits in LimitedNonZeroU8<100>");
    let params = list_notifications::ParametersData {
        cursor,
        limit: Some(limit),
        priority: None,
        reasons: None,
        seen_at: None,
    }
    .into();
    match block_on(
        client
            .agent
            .api
            .app
            .bsky
            .notification
            .list_notifications(params),
    ) {
        Ok(o) => WorkResponse::Notifications(Ok(NotificationBatch {
            notifications: o.data.notifications,
            cursor: o.data.cursor,
        })),
        Err(e) => WorkResponse::Notifications(Err(format!("{e}"))),
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

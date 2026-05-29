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
    FeedViewPost, GeneratorView, PostView, ThreadViewPostParentRefs, ThreadViewPostRepliesItem,
};
use atrium_api::app::bsky::feed::{
    get_actor_feeds, get_actor_likes, get_author_feed, get_feed, get_feed_generators,
    get_post_thread, get_timeline,
};
use atrium_api::app::bsky::feed::get_post_thread::OutputThreadRefs;
use atrium_api::app::bsky::graph::defs::{ListView, StarterPackViewBasic};
use atrium_api::app::bsky::graph::{get_actor_starter_packs, get_lists};
use atrium_api::app::bsky::feed::like::RecordData as LikeRecordData;
use atrium_api::app::bsky::feed::post::{RecordData as PostRecordData, ReplyRef, ReplyRefData};
use atrium_api::app::bsky::feed::repost::RecordData as RepostRecordData;
use atrium_api::app::bsky::graph::follow::RecordData as FollowRecordData;
use atrium_api::app::bsky::notification::list_notifications;
use atrium_api::app::bsky::notification::list_notifications::Notification;
use atrium_api::app::bsky::notification::update_seen;
use atrium_api::com::atproto::repo::{create_record, delete_record};
use atrium_api::agent::bluesky::{AtprotoServiceType, BSKY_CHAT_DID};
use atrium_api::chat::bsky::convo::defs::{
    ConvoView, DeletedMessageView, MessageInputData, MessageView,
};
use atrium_api::chat::bsky::convo::get_messages::OutputMessagesItem;
use atrium_api::chat::bsky::convo::{
    get_convo_for_members, get_messages, list_convos, send_message, update_read,
};
use atrium_api::types::string::{AtIdentifier, Datetime, Did, Nsid, RecordKey};
use atrium_api::types::{LimitedNonZeroU8, LimitedU16, Union, Unknown};
use atrium_xrpc::http::Request;
use atrium_xrpc::HttpClient;
use bsky_auth::{agent_call, AuthClient};
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
    /// Posts authored by `actor` — `getAuthorFeed` with
    /// `filter="posts_and_author_threads"`. Includes the pinned post
    /// inline as the first item (with a `ReasonPin` reason) when
    /// `cursor` is `None`. Used by ProfileScreen's Posts tab.
    AuthorPosts { actor: String },
    /// Posts + replies authored by `actor` — `getAuthorFeed` with
    /// `filter="posts_with_replies"`. Used by ProfileScreen's Replies tab.
    AuthorReplies { actor: String },
    /// Posts with media (images / video) authored by `actor` —
    /// `getAuthorFeed` with `filter="posts_with_media"`. Used by
    /// ProfileScreen's Media tab.
    AuthorMedia { actor: String },
    /// Posts liked by `actor` — `getActorLikes`. Server enforces
    /// own-profile-only visibility. Used by ProfileScreen's Likes tab.
    AuthorLikes { actor: String },
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
    /// Read a LOCAL image file off the Vita filesystem (for picker
    /// thumbnails + the compose preview). Reuses the `Image` response,
    /// keyed by the file path instead of a URL — the main thread tells
    /// local reads from network fetches by the non-`http` key prefix.
    ReadImageFile { path: String },
    /// Create a post, or a thread of connected self-replies, via
    /// `com.atproto.repo.createRecord`. `segments` has ≥1 entry; >1 posts
    /// a chain where each is a reply to the previous (shared root).
    /// `reply_to: None` ⇒ new top-level thread; `Some(_)` ⇒ the whole
    /// thread hangs off that target post. Replies with `PostCreated`
    /// carrying the first post's URI.
    CreateThread {
        segments: Vec<ThreadSegment>,
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
    /// Download a video blob via `com.atproto.sync.getBlob` and write
    /// it to a local file. atrium routes the call to the right PDS;
    /// the handler skips the network round-trip if the file is
    /// already cached on disk under `<DATA_DIR>/video/<cid>.mp4`.
    FetchVideoBlob { did: String, cid: String },
    /// Fetch a page of feed generators (custom feeds) created by
    /// `actor` via `app.bsky.feed.getActorFeeds`. Used by
    /// ProfileScreen's Feeds tab. `actor` is the resolved DID.
    FetchActorFeeds { actor: String, cursor: Option<String> },
    /// Fetch a page of lists curated by `actor` via
    /// `app.bsky.graph.getLists`. Used by ProfileScreen's Lists tab.
    FetchActorLists { actor: String, cursor: Option<String> },
    /// Fetch a page of starter packs created by `actor` via
    /// `app.bsky.graph.getActorStarterPacks`. Used by
    /// ProfileScreen's Packs tab.
    FetchActorStarterPacks { actor: String, cursor: Option<String> },
    /// List the user's DM conversations via `chat.bsky.convo.listConvos`
    /// (most-recent-activity first). Cursor-paginated.
    FetchConvos { cursor: Option<String> },
    /// Fetch a page of messages in `convo_id` via
    /// `chat.bsky.convo.getMessages`. `cursor: None` = newest page;
    /// the cursor pages backward into older history. `convo_id` echoes
    /// back so a screen can drop responses for a convo it switched away
    /// from.
    GetConvoMessages {
        convo_id: String,
        cursor: Option<String>,
    },
    /// Send `text` to `convo_id` via `chat.bsky.convo.sendMessage`.
    SendMessage { convo_id: String, text: String },
    /// Mark `convo_id` read via `chat.bsky.convo.updateRead`.
    /// Fire-and-forget (the `ConvoRead` response is ignored by screens).
    MarkConvoRead { convo_id: String },
    /// Get (or create) the conversation with `members` (DIDs) via
    /// `chat.bsky.convo.getConvoForMembers`. Used by the profile
    /// "Message" button to open a 1:1 chat.
    GetConvoForMembers { members: Vec<String> },
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

/// One image to attach to a post: pre-encoded bytes (PNG or JPEG) plus
/// its MIME type. `alt` is accessibility text (empty in v1). The worker
/// uploads the bytes via `uploadBlob` then embeds the returned blob ref.
#[derive(Clone, Debug)]
pub struct ComposedImage {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub alt: String,
}

/// One post within a thread: its text + attached images.
#[derive(Clone, Debug)]
pub struct ThreadSegment {
    pub text: String,
    pub images: Vec<ComposedImage>,
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

/// One page of feed generators (custom feeds) created by an actor.
pub struct ActorFeedsBatch {
    pub feeds: Vec<GeneratorView>,
    pub cursor: Option<String>,
}

/// One page of lists curated by an actor.
pub struct ActorListsBatch {
    pub lists: Vec<ListView>,
    pub cursor: Option<String>,
}

/// One page of starter packs created by an actor.
pub struct ActorStarterPacksBatch {
    pub starter_packs: Vec<StarterPackViewBasic>,
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

/// One page of DM conversations + the next-page cursor (None = end).
pub struct ConvosBatch {
    pub convos: Vec<ConvoView>,
    pub cursor: Option<String>,
}

/// One message in a conversation — either a live message or a tombstone
/// for a deleted one. Decoded from atrium's `getMessages` union in the
/// worker so the UI never touches `Union`.
#[derive(Clone)]
pub enum MessageItem {
    Message(MessageView),
    Deleted(DeletedMessageView),
}

/// One page of conversation messages. `messages` is normalized to
/// **oldest → newest** (the chat API returns newest-first; the worker
/// reverses each page). `cursor` pages backward into older history
/// (None = reached the start of the conversation).
pub struct MessagesBatch {
    pub messages: Vec<MessageItem>,
    pub cursor: Option<String>,
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
    /// `Ok(file_path)` when the video blob is downloaded (or was
    /// already cached). The caller (`VideoPlayerScreen`) opens the
    /// file with `sceAvPlayer`.
    VideoBlob {
        cid: String,
        result: Result<String, String>,
    },
    /// One page of an actor's custom feeds. `actor` is the resolved DID
    /// — used by ProfileScreen as the staleness key (drops responses
    /// that arrive after the user has navigated to a different profile).
    ActorFeeds {
        actor: String,
        batch: Result<ActorFeedsBatch, String>,
    },
    /// One page of an actor's curated lists.
    ActorLists {
        actor: String,
        batch: Result<ActorListsBatch, String>,
    },
    /// One page of an actor's starter packs.
    ActorStarterPacks {
        actor: String,
        batch: Result<ActorStarterPacksBatch, String>,
    },
    /// One page of the user's DM conversations.
    Convos(Result<ConvosBatch, String>),
    /// One page of messages for `convo_id` (oldest→newest). `convo_id`
    /// is the staleness key so a screen drops pages for a convo it has
    /// navigated away from.
    ConvoMessages {
        convo_id: String,
        batch: Result<MessagesBatch, String>,
    },
    /// The server's view of a just-sent message, for the conversation
    /// screen to reconcile against its optimistic local row.
    MessageSent {
        convo_id: String,
        result: Result<MessageView, String>,
    },
    /// The conversation for a `GetConvoForMembers` request (profile
    /// "Message" button) — carries the convo to open.
    ConvoForMembers(Result<ConvoView, String>),
    /// Ack for a fire-and-forget `MarkConvoRead`. Ignored by screens;
    /// surfaced only so errors land in the log.
    ConvoRead(Result<(), String>),
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
        WorkResponse::VideoBlob { cid, result: Err(e) } => {
            bsky_log::log!("worker: VideoBlob({cid}) err: {e}")
        }
        WorkResponse::ActorFeeds { actor, batch: Err(e) } => {
            bsky_log::log!("worker: ActorFeeds({actor}) err: {e}")
        }
        WorkResponse::ActorLists { actor, batch: Err(e) } => {
            bsky_log::log!("worker: ActorLists({actor}) err: {e}")
        }
        WorkResponse::ActorStarterPacks { actor, batch: Err(e) } => {
            bsky_log::log!("worker: ActorStarterPacks({actor}) err: {e}")
        }
        WorkResponse::Convos(Err(e)) => bsky_log::log!("worker: Convos err: {e}"),
        WorkResponse::ConvoMessages { convo_id, batch: Err(e) } => {
            bsky_log::log!("worker: ConvoMessages({convo_id}) err: {e}")
        }
        WorkResponse::MessageSent { convo_id, result: Err(e) } => {
            bsky_log::log!("worker: MessageSent({convo_id}) err: {e}")
        }
        WorkResponse::ConvoForMembers(Err(e)) => {
            bsky_log::log!("worker: ConvoForMembers err: {e}")
        }
        WorkResponse::ConvoRead(Err(e)) => bsky_log::log!("worker: ConvoRead err: {e}"),
        _ => {}
    }
}

fn handle_request(client: &AuthClient, req: WorkRequest) -> WorkResponse {
    match req {
        WorkRequest::FetchProfile { actor } => {
            // Resolve actor: None ⇒ session's own DID; Some(s) ⇒ use directly.
            let actor_str = match actor {
                Some(a) => a,
                None => match agent_call!(client, |a| a.did()) {
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
            let result = agent_call!(client, |a| a.api.app.bsky.actor.get_profile(
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
        WorkRequest::ReadImageFile { path } => {
            let bytes = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"));
            WorkResponse::Image { url: path, bytes }
        }
        WorkRequest::CreateThread { segments, reply_to } => {
            create_thread(client, segments, reply_to)
        }
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
        WorkRequest::FetchVideoBlob { did, cid } => fetch_video_blob(client, did, cid),
        WorkRequest::FetchActorFeeds { actor, cursor } => {
            fetch_actor_feeds(client, actor, cursor)
        }
        WorkRequest::FetchActorLists { actor, cursor } => {
            fetch_actor_lists(client, actor, cursor)
        }
        WorkRequest::FetchActorStarterPacks { actor, cursor } => {
            fetch_actor_starter_packs(client, actor, cursor)
        }
        WorkRequest::FetchNotifications { cursor } => fetch_notifications(client, cursor),
        WorkRequest::MarkSeen { seen_at } => {
            // Fire-and-forget; the result is logged but not surfaced.
            let input = update_seen::InputData { seen_at }.into();
            if let Err(e) =
                agent_call!(client, |a| a.api.app.bsky.notification.update_seen(input))
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
        WorkRequest::FetchConvos { cursor } => fetch_convos(client, cursor),
        WorkRequest::GetConvoMessages { convo_id, cursor } => {
            fetch_convo_messages(client, convo_id, cursor)
        }
        WorkRequest::SendMessage { convo_id, text } => {
            send_chat_message(client, convo_id, text)
        }
        WorkRequest::MarkConvoRead { convo_id } => mark_convo_read(client, convo_id),
        WorkRequest::GetConvoForMembers { members } => {
            fetch_convo_for_members(client, members)
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
    match agent_call!(client, |a| a.api.app.bsky.actor.search_actors(params)) {
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
    match agent_call!(client, |a| a.api.app.bsky.feed.search_posts(params)) {
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
            match agent_call!(client, |a| a.api.app.bsky.feed.get_timeline(params)) {
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
            match agent_call!(client, |a| a.api.app.bsky.feed.get_feed(params)) {
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
        FeedSource::AuthorPosts { .. }
        | FeedSource::AuthorReplies { .. }
        | FeedSource::AuthorMedia { .. } => {
            let (actor, filter) = match &source {
                FeedSource::AuthorPosts { actor } => (actor.clone(), "posts_and_author_threads"),
                FeedSource::AuthorReplies { actor } => (actor.clone(), "posts_with_replies"),
                FeedSource::AuthorMedia { actor } => (actor.clone(), "posts_with_media"),
                _ => unreachable!(),
            };
            let at_id = match AtIdentifier::from_str(&actor) {
                Ok(id) => id,
                Err(e) => {
                    return WorkResponse::FeedPage {
                        source,
                        batch: Err(format!("invalid actor {actor:?}: {e}")),
                    };
                }
            };
            // include_pins only matters for the first page, and only
            // for posts_and_author_threads (the bsky-app convention).
            let include_pins = matches!(source, FeedSource::AuthorPosts { .. })
                && cursor.is_none();
            let params = get_author_feed::ParametersData {
                actor: at_id,
                cursor,
                filter: Some(filter.to_string()),
                include_pins: Some(include_pins),
                limit: Some(limit),
            }
            .into();
            match agent_call!(client, |a| a.api.app.bsky.feed.get_author_feed(params)) {
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
        FeedSource::AuthorLikes { actor } => {
            let actor = actor.clone();
            let at_id = match AtIdentifier::from_str(&actor) {
                Ok(id) => id,
                Err(e) => {
                    return WorkResponse::FeedPage {
                        source,
                        batch: Err(format!("invalid actor {actor:?}: {e}")),
                    };
                }
            };
            let params = get_actor_likes::ParametersData {
                actor: at_id,
                cursor,
                limit: Some(limit),
            }
            .into();
            match agent_call!(client, |a| a.api.app.bsky.feed.get_actor_likes(params)) {
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

/// Fetch a page of custom feeds (`GeneratorView`s) created by `actor`.
/// `actor` is echoed in the response as the staleness key.
fn fetch_actor_feeds(
    client: &AuthClient,
    actor: String,
    cursor: Option<String>,
) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(50)
        .expect("50 fits in LimitedNonZeroU8<100>");
    let at_id = match AtIdentifier::from_str(&actor) {
        Ok(id) => id,
        Err(e) => {
            return WorkResponse::ActorFeeds {
                actor,
                batch: Err(format!("invalid actor: {e}")),
            };
        }
    };
    let params = get_actor_feeds::ParametersData {
        actor: at_id,
        cursor,
        limit: Some(limit),
    }
    .into();
    match agent_call!(client, |a| a.api.app.bsky.feed.get_actor_feeds(params)) {
        Ok(o) => WorkResponse::ActorFeeds {
            actor,
            batch: Ok(ActorFeedsBatch {
                feeds: o.data.feeds,
                cursor: o.data.cursor,
            }),
        },
        Err(e) => WorkResponse::ActorFeeds {
            actor,
            batch: Err(format!("{e}")),
        },
    }
}

/// Fetch a page of lists curated by `actor`.
fn fetch_actor_lists(
    client: &AuthClient,
    actor: String,
    cursor: Option<String>,
) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(50)
        .expect("50 fits in LimitedNonZeroU8<100>");
    let at_id = match AtIdentifier::from_str(&actor) {
        Ok(id) => id,
        Err(e) => {
            return WorkResponse::ActorLists {
                actor,
                batch: Err(format!("invalid actor: {e}")),
            };
        }
    };
    let params = get_lists::ParametersData {
        actor: at_id,
        cursor,
        limit: Some(limit),
        purposes: None,
    }
    .into();
    match agent_call!(client, |a| a.api.app.bsky.graph.get_lists(params)) {
        Ok(o) => WorkResponse::ActorLists {
            actor,
            batch: Ok(ActorListsBatch {
                lists: o.data.lists,
                cursor: o.data.cursor,
            }),
        },
        Err(e) => WorkResponse::ActorLists {
            actor,
            batch: Err(format!("{e}")),
        },
    }
}

/// Fetch a page of starter packs created by `actor`.
fn fetch_actor_starter_packs(
    client: &AuthClient,
    actor: String,
    cursor: Option<String>,
) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(50)
        .expect("50 fits in LimitedNonZeroU8<100>");
    let at_id = match AtIdentifier::from_str(&actor) {
        Ok(id) => id,
        Err(e) => {
            return WorkResponse::ActorStarterPacks {
                actor,
                batch: Err(format!("invalid actor: {e}")),
            };
        }
    };
    let params = get_actor_starter_packs::ParametersData {
        actor: at_id,
        cursor,
        limit: Some(limit),
    }
    .into();
    match agent_call!(client, |a| a.api.app.bsky.graph.get_actor_starter_packs(params)) {
        Ok(o) => WorkResponse::ActorStarterPacks {
            actor,
            batch: Ok(ActorStarterPacksBatch {
                starter_packs: o.data.starter_packs,
                cursor: o.data.cursor,
            }),
        },
        Err(e) => WorkResponse::ActorStarterPacks {
            actor,
            batch: Err(format!("{e}")),
        },
    }
}

/// Read the user's pinned-feeds preference + hydrate generator metadata
/// in one round-trip. Output always begins with a `Following` pin (we
/// synthesize one if it's not in the prefs).
fn fetch_saved_feeds(client: &AuthClient) -> WorkResponse {
    // 1. Read prefs.
    let params = get_preferences::ParametersData {}.into();
    let prefs = match agent_call!(client, |a| a.api.app.bsky.actor.get_preferences(params)) {
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
        match agent_call!(client, |a| a.api.app.bsky.feed.get_feed_generators(params)) {
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
    match agent_call!(client, |a| a
            .api
            .app
            .bsky
            .notification
            .list_notifications(params))
    {
        Ok(o) => WorkResponse::Notifications(Ok(NotificationBatch {
            notifications: o.data.notifications,
            cursor: o.data.cursor,
        })),
        Err(e) => WorkResponse::Notifications(Err(format!("{e}"))),
    }
}

// ─── Direct messages (chat.bsky.convo.*) ─────────────────────────────
//
// DM endpoints are NOT served by the user's PDS directly — they're
// proxied to the Bluesky chat service. atrium injects the required
// `atproto-proxy: did:web:api.bsky.chat#bsky_chat` header when the call
// is routed through `api_with_proxy(BSKY_CHAT_DID, BskyChat)`. The chat
// lexicon is compiled in via the `bluesky` feature (→ namespace-chatbsky),
// so no extra Cargo feature is needed.
//
// NOTE: an app password created without "Allow access to your direct
// messages" fails every chat call with a bad-token-scope error.
// `map_chat_err` tags those with a `DM_SCOPE:` sentinel so screens can
// show an actionable message instead of a generic failure.

/// The chat service's DID, parsed once per call (cheap; const string).
fn chat_did() -> Did {
    Did::from_str(BSKY_CHAT_DID).expect("BSKY_CHAT_DID is a valid DID")
}

/// Stringify a chat XRPC error, tagging app-password-scope failures with
/// a `DM_SCOPE:` prefix the UI keys off. (Flagged: the exact wording is
/// verified on hardware — broaden the match if the real error differs.)
fn map_chat_err(e: impl std::fmt::Display) -> String {
    let s = format!("{e}");
    let l = s.to_ascii_lowercase();
    if l.contains("scope") || l.contains("bad token") || l.contains("invalidtoken") {
        format!("DM_SCOPE: {s}")
    } else {
        s
    }
}

/// Decode one `getMessages` union item into our flat `MessageItem`.
/// Drops unknown variants (forward-compat with future message kinds).
fn decode_message_item(u: Union<OutputMessagesItem>) -> Option<MessageItem> {
    match u {
        Union::Refs(OutputMessagesItem::ChatBskyConvoDefsMessageView(m)) => {
            Some(MessageItem::Message(*m))
        }
        Union::Refs(OutputMessagesItem::ChatBskyConvoDefsDeletedMessageView(d)) => {
            Some(MessageItem::Deleted(*d))
        }
        Union::Unknown(_) => None,
    }
}

/// List the user's conversations (most-recent-activity first).
fn fetch_convos(client: &AuthClient, cursor: Option<String>) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(50)
        .expect("50 fits in LimitedNonZeroU8<100>");
    let params = list_convos::ParametersData {
        cursor,
        limit: Some(limit),
        read_state: None,
        status: None,
    }
    .into();
    match agent_call!(client, |a| a
            .api_with_proxy(chat_did(), AtprotoServiceType::BskyChat)
            .chat
            .bsky
            .convo
            .list_convos(params))
    {
        Ok(o) => WorkResponse::Convos(Ok(ConvosBatch {
            convos: o.data.convos,
            cursor: o.data.cursor,
        })),
        Err(e) => WorkResponse::Convos(Err(map_chat_err(e))),
    }
}

/// Fetch a page of messages for `convo_id`. The chat API returns messages
/// newest-first and the cursor pages *backward* into older history; we
/// reverse each page to oldest→newest so screens never reason about API
/// order. `convo_id` is echoed back as the staleness key.
fn fetch_convo_messages(
    client: &AuthClient,
    convo_id: String,
    cursor: Option<String>,
) -> WorkResponse {
    let limit = LimitedNonZeroU8::<100>::try_from(50)
        .expect("50 fits in LimitedNonZeroU8<100>");
    let params = get_messages::ParametersData {
        convo_id: convo_id.clone(),
        cursor,
        limit: Some(limit),
    }
    .into();
    match agent_call!(client, |a| a
            .api_with_proxy(chat_did(), AtprotoServiceType::BskyChat)
            .chat
            .bsky
            .convo
            .get_messages(params))
    {
        Ok(o) => {
            let mut messages: Vec<MessageItem> = o
                .data
                .messages
                .into_iter()
                .filter_map(decode_message_item)
                .collect();
            messages.reverse(); // newest-first → oldest→newest
            WorkResponse::ConvoMessages {
                convo_id,
                batch: Ok(MessagesBatch {
                    messages,
                    cursor: o.data.cursor,
                }),
            }
        }
        Err(e) => WorkResponse::ConvoMessages {
            convo_id,
            batch: Err(map_chat_err(e)),
        },
    }
}

/// Send a text message to `convo_id`. Returns the server's `MessageView`
/// so the screen can reconcile its optimistic local row.
fn send_chat_message(client: &AuthClient, convo_id: String, text: String) -> WorkResponse {
    let message = MessageInputData {
        embed: None,
        facets: None,
        text,
    }
    .into();
    let input = send_message::InputData {
        convo_id: convo_id.clone(),
        message,
    }
    .into();
    match agent_call!(client, |a| a
            .api_with_proxy(chat_did(), AtprotoServiceType::BskyChat)
            .chat
            .bsky
            .convo
            .send_message(input))
    {
        Ok(view) => WorkResponse::MessageSent {
            convo_id,
            result: Ok(view),
        },
        Err(e) => WorkResponse::MessageSent {
            convo_id,
            result: Err(map_chat_err(e)),
        },
    }
}

/// Mark `convo_id` read up to its latest message. Fire-and-forget — the
/// `ConvoRead` response is ignored by screens (logged on error only).
fn mark_convo_read(client: &AuthClient, convo_id: String) -> WorkResponse {
    let input = update_read::InputData {
        convo_id,
        message_id: None,
    }
    .into();
    match agent_call!(client, |a| a
            .api_with_proxy(chat_did(), AtprotoServiceType::BskyChat)
            .chat
            .bsky
            .convo
            .update_read(input))
    {
        Ok(_) => WorkResponse::ConvoRead(Ok(())),
        Err(e) => WorkResponse::ConvoRead(Err(map_chat_err(e))),
    }
}

/// Get (or create) the conversation with `members` (DIDs). Used by the
/// profile "Message" button to open a chat with an arbitrary user.
fn fetch_convo_for_members(client: &AuthClient, members: Vec<String>) -> WorkResponse {
    let dids: Result<Vec<Did>, String> = members
        .iter()
        .map(|m| Did::from_str(m).map_err(|e| format!("member DID {m:?}: {e}")))
        .collect();
    let members = match dids {
        Ok(d) => d,
        Err(e) => return WorkResponse::ConvoForMembers(Err(e)),
    };
    let params = get_convo_for_members::ParametersData { members }.into();
    match agent_call!(client, |a| a
            .api_with_proxy(chat_did(), AtprotoServiceType::BskyChat)
            .chat
            .bsky
            .convo
            .get_convo_for_members(params))
    {
        Ok(o) => WorkResponse::ConvoForMembers(Ok(o.data.convo)),
        Err(e) => WorkResponse::ConvoForMembers(Err(map_chat_err(e))),
    }
}

/// Create an `app.bsky.graph.follow` record targeting `actor_did`.
fn create_follow(client: &AuthClient, actor_did: String) -> WorkResponse {
    let did_str = match agent_call!(client, |a| a.did()) {
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
    match agent_call!(client, |a| a.api.com.atproto.repo.create_record(input.into())) {
        Ok(o) => WorkResponse::FollowChanged(Ok(Some(o.data.uri))),
        Err(e) => WorkResponse::FollowChanged(Err(format!("{e}"))),
    }
}

fn delete_follow(client: &AuthClient, rkey_str: &str) -> WorkResponse {
    let did_str = match agent_call!(client, |a| a.did()) {
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
    match agent_call!(client, |a| a.api.com.atproto.repo.delete_record(input.into())) {
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
    match agent_call!(client, |a| a.api.app.bsky.feed.get_post_thread(params)) {
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

    let did_str = match agent_call!(client, |a| a.did()) {
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
    match agent_call!(client, |a| a.api.com.atproto.repo.create_record(input.into())) {
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
    let did_str = match agent_call!(client, |a| a.did()) {
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
    match agent_call!(client, |a| a.api.com.atproto.repo.delete_record(input.into())) {
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

/// Build a typed `ReplyRef` from root/parent (uri, cid) strong refs.
fn build_reply_ref(
    root_uri: String,
    root_cid: atrium_api::types::string::Cid,
    parent_uri: String,
    parent_cid: atrium_api::types::string::Cid,
) -> ReplyRef {
    use atrium_api::com::atproto::repo::strong_ref::MainData as StrongRefData;
    ReplyRefData {
        parent: StrongRefData {
            cid: parent_cid,
            uri: parent_uri,
        }
        .into(),
        root: StrongRefData {
            cid: root_cid,
            uri: root_uri,
        }
        .into(),
    }
    .into()
}

/// Create one `app.bsky.feed.post` record (uploading + embedding its
/// images). Returns the new post's `(uri, cid)` for chaining.
fn create_one_post(
    client: &AuthClient,
    repo: AtIdentifier,
    text: String,
    reply: Option<ReplyRef>,
    images: Vec<ComposedImage>,
) -> Result<(String, atrium_api::types::string::Cid), String> {
    // Upload each image as a blob, then build the images embed.
    let embed = if images.is_empty() {
        None
    } else {
        use atrium_api::app::bsky::embed::images::{ImageData, Main, MainData};
        use atrium_api::app::bsky::feed::post::RecordEmbedRefs;

        let mut items = Vec::with_capacity(images.len());
        for img in images {
            let blob = agent_call!(client, |a| a.api.com.atproto.repo.upload_blob(img.bytes))
                .map_err(|e| format!("uploadBlob: {e}"))?
                .data
                .blob;
            items.push(
                ImageData {
                    alt: img.alt,
                    aspect_ratio: None,
                    image: blob,
                }
                .into(),
            );
        }
        let main: Main = MainData { images: items }.into();
        Some(Union::Refs(RecordEmbedRefs::AppBskyEmbedImagesMain(
            Box::new(main),
        )))
    };

    let record = PostRecordData {
        text,
        created_at: Datetime::now(),
        reply,
        embed,
        entities: None,
        facets: None,
        labels: None,
        langs: None,
        tags: None,
    };

    // atrium's typed RecordData omits `$type` — inject it via a serde_json
    // round-trip before converting to the wire-shape `Unknown`.
    let mut json = serde_json::to_value(&record).map_err(|e| format!("serialize: {e}"))?;
    if let serde_json::Value::Object(map) = &mut json {
        map.insert(
            "$type".to_string(),
            serde_json::Value::String("app.bsky.feed.post".to_string()),
        );
    } else {
        return Err("post record didn't serialize as a JSON object".into());
    }
    let unknown: Unknown =
        serde_json::from_value(json).map_err(|e| format!("re-deserialize: {e}"))?;
    let collection = Nsid::from_str("app.bsky.feed.post").map_err(|e| format!("nsid parse: {e}"))?;
    let input = create_record::InputData {
        collection,
        record: unknown,
        repo,
        rkey: None,
        swap_commit: None,
        validate: None,
    };
    let o = agent_call!(client, |a| a.api.com.atproto.repo.create_record(input.into()))
        .map_err(|e| format!("{e}"))?;
    Ok((o.data.uri, o.data.cid))
}

/// Post one-or-more connected segments as a thread. Each segment after
/// the first is a self-reply to the previous; all share one root. If
/// `reply_to` is set, the whole thread hangs off that target post.
/// Returns the first post's AT-URI.
fn create_thread(
    client: &AuthClient,
    segments: Vec<ThreadSegment>,
    reply_to: Option<ReplyTarget>,
) -> WorkResponse {
    use atrium_api::types::string::Cid;
    if segments.is_empty() {
        return WorkResponse::PostCreated(Err("empty thread".into()));
    }
    let did_str = match agent_call!(client, |a| a.did()) {
        Some(d) => d.to_string(),
        None => return WorkResponse::PostCreated(Err("not logged in".into())),
    };
    let repo = match AtIdentifier::from_str(&did_str) {
        Ok(id) => id,
        Err(e) => return WorkResponse::PostCreated(Err(format!("DID parse: {e}"))),
    };

    // Seed root/parent from the reply target, if any.
    let (mut root, mut parent): (Option<(String, Cid)>, Option<(String, Cid)>) = match reply_to {
        Some(rt) => {
            let root_cid = match Cid::from_str(&rt.root_cid) {
                Ok(c) => c,
                Err(e) => return WorkResponse::PostCreated(Err(format!("root cid: {e}"))),
            };
            let parent_cid = match Cid::from_str(&rt.parent_cid) {
                Ok(c) => c,
                Err(e) => return WorkResponse::PostCreated(Err(format!("parent cid: {e}"))),
            };
            (
                Some((rt.root_uri, root_cid)),
                Some((rt.parent_uri, parent_cid)),
            )
        }
        None => (None, None),
    };

    let total = segments.len();
    let mut first_uri: Option<String> = None;
    for (i, seg) in segments.into_iter().enumerate() {
        let reply = match (&root, &parent) {
            (Some((ru, rc)), Some((pu, pc))) => {
                Some(build_reply_ref(ru.clone(), rc.clone(), pu.clone(), pc.clone()))
            }
            // First post of a brand-new top-level thread: no reply ref.
            _ => None,
        };
        let (uri, cid) =
            match create_one_post(client, repo.clone(), seg.text, reply, seg.images) {
                Ok(v) => v,
                Err(e) => {
                    let msg = if i == 0 {
                        e
                    } else {
                        format!("posted {i}/{total}, then failed: {e}")
                    };
                    return WorkResponse::PostCreated(Err(msg));
                }
            };
        if first_uri.is_none() {
            first_uri = Some(uri.clone());
        }
        // For a new top-level thread, the first post becomes the root.
        if root.is_none() {
            root = Some((uri.clone(), cid.clone()));
        }
        // Every subsequent segment replies to this one.
        parent = Some((uri, cid));
    }
    WorkResponse::PostCreated(Ok(first_uri.unwrap_or_default()))
}

/// Download a video blob via `com.atproto.sync.getBlob` and write it
/// to `<DATA_DIR>/video/<cid>.mp4`. `getBlob` is implemented by the
/// post author's PDS — we resolve the author's DID → PDS first
/// (atrium's authenticated agent only knows the *user's* PDS), then
/// GET the blob with `VitaHttpClient` (auth-free). If the file
/// already exists with non-zero size, skip the network entirely.
fn fetch_video_blob(_client: &AuthClient, did_str: String, cid_str: String) -> WorkResponse {
    let dir = format!("{}/video", bsky_auth::DATA_DIR);
    let path = format!("{}/{}.mp4", dir, cid_str);

    // Cache hit?
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 0 {
            return WorkResponse::VideoBlob {
                cid: cid_str,
                result: Ok(path),
            };
        }
    }

    // Resolve the author's PDS — atrium's identity resolver accepts a
    // DID directly and walks the PLC/web doc to find the PDS service.
    bsky_log::log!("VideoBlob: resolving PDS for did={did_str}");
    let http = std::sync::Arc::new(VitaHttpClient::new());
    let resolved = match block_on(bsky_auth::resolve_pds(
        std::sync::Arc::clone(&http),
        &did_str,
    )) {
        Ok(r) => r,
        Err(e) => {
            return WorkResponse::VideoBlob {
                cid: cid_str,
                result: Err(format!("resolve pds: {e}")),
            }
        }
    };
    bsky_log::log!("VideoBlob: pds={}", resolved.pds);

    // Construct + GET the URL. `:` in DIDs is RFC 3986-allowed in
    // query strings without percent-encoding; CIDs are base32 (no
    // reserved chars).
    let url = format!(
        "{}/xrpc/com.atproto.sync.getBlob?did={}&cid={}",
        resolved.pds.trim_end_matches('/'),
        did_str,
        cid_str,
    );
    bsky_log::log!("VideoBlob: fetching {url}");
    let bytes = match fetch_image_bytes(&url) {
        Ok(b) => b,
        Err(e) => {
            return WorkResponse::VideoBlob {
                cid: cid_str,
                result: Err(format!("{e}")),
            }
        }
    };
    bsky_log::log!("VideoBlob: fetched {} bytes", bytes.len());

    if let Err(e) = std::fs::create_dir_all(&dir) {
        return WorkResponse::VideoBlob {
            cid: cid_str,
            result: Err(format!("mkdir {dir}: {e}")),
        };
    }
    if let Err(e) = std::fs::write(&path, &bytes) {
        return WorkResponse::VideoBlob {
            cid: cid_str,
            result: Err(format!("write {path}: {e}")),
        };
    }
    bsky_log::log!("VideoBlob cached: {path} ({} bytes)", bytes.len());
    WorkResponse::VideoBlob {
        cid: cid_str,
        result: Ok(path),
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

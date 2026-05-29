//! Profile screen — shows the logged-in user's own profile.
//!
//! Phase 3.1 worker pattern:
//! 1. First frame after construction: dispatch `WorkRequest::GetOwnProfile`
//!    to the worker, mark `dispatched = true`, render "Loading profile…".
//! 2. Subsequent frames: still render "Loading…" while `state == Pending`.
//! 3. Worker eventually publishes a `WorkResponse::Profile` — main.rs
//!    drains it via `try_recv` and calls `handle_worker_response`, which
//!    flips `state` to `Loaded` or `Error`.
//! 4. Next frame: render display name + handle + counts.
//!
//! No logout / no refresh actions in 3.1 — Phase 3+ polish.

use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_api::app::bsky::actor::defs::ProfileViewDetailedData;
use atrium_api::app::bsky::feed::defs::{FeedViewPost, FeedViewPostReasonRefs, GeneratorView};
use atrium_api::chat::bsky::convo::defs::ConvoView;
use atrium_api::app::bsky::graph::defs::{ListView, StarterPackViewBasic};
use atrium_api::types::Union;
use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_render::{theme, Color, Font, Frame, Texture, TextureCache, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{FeedSource, WorkRequest, WorkResponse};

use bsky_input::buttons;

use crate::compose::ComposeScreen;
use crate::conversation::ConversationScreen;
use crate::thread::ThreadScreen;
use crate::timeline::{detect_post_tap_action, toggle_engagement, EngagementKind, TapAction};
use crate::video_player::VideoPlayerScreen;
use bsky_worker::ReplyTarget;

use crate::cdn::{avatar_thumbnail_jpeg, banner_jpeg};
use crate::screen::{Screen, ScreenAction};
use crate::tabbar::{TabBar, TopLevel};
use crate::widget::{button, ButtonState, Rect, UiCtx};

const AVATAR_SIZE: i32 = 96;
/// Banner strip height in screen pixels. Bluesky banners are typically
/// 3:1 source aspect; we render them in an 8:1 strip (960×120) and
/// center-crop vertically.
const BANNER_H: i32 = 120;
/// Top-of-avatar y. Avatar overlaps the banner's lower portion.
const AVATAR_Y: i32 = 80;
/// Natural y of the sub-tab pill strip. When `scroll_y` exceeds this,
/// the strip clamps to y=0 and the tab content scrolls under it.
const TAB_STRIP_Y: i32 = 374;
/// Sub-tab pill strip height (pill bg + bottom accent bar combined).
const TAB_STRIP_H: i32 = 40;
/// Analog-stick scrolling, mirroring TimelineScreen's constants.
const STICK_DEADZONE: i8 = 32;
const STICK_DIVISOR: f32 = 24.0;
/// Trigger pagination when within this many px of the end of the
/// active tab's content. Mirrors TimelineScreen.
const PAGINATION_THRESHOLD: i32 = 600;
/// Bottom-of-viewport y for own-profile (clipped above the global
/// 60 px tab bar). Other-actor profiles can render down to SCREEN_HEIGHT.
const OWN_VIEWPORT_BOTTOM: i32 = SCREEN_HEIGHT - 60;

/// Sub-tabs on the expanded profile, mirroring bsky-app's mobile UX.
/// Each renders its own scrollable content area below the pill strip;
/// per-tab state (posts/cursor/scroll/selected) lives on `ProfileScreen`
/// so re-tapping a tab is instant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProfileTab {
    Posts,
    Replies,
    Media,
    Likes,
    Feeds,
    Lists,
    Packs,
}

impl ProfileTab {
    const ALL: [Self; 7] = [
        Self::Posts,
        Self::Replies,
        Self::Media,
        Self::Likes,
        Self::Feeds,
        Self::Lists,
        Self::Packs,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Posts => "Posts",
            Self::Replies => "Replies",
            Self::Media => "Media",
            Self::Likes => "Likes",
            Self::Feeds => "Feeds",
            Self::Lists => "Lists",
            Self::Packs => "Packs",
        }
    }
}

/// Per-tab state for one of the post-shaped tabs (Posts / Replies /
/// Media / Likes). Each tab owns its own scroll / selection / cursor
/// so re-tapping is instant.
#[derive(Default)]
struct TabFeedState {
    posts: Vec<FeedViewPost>,
    /// Index into `posts` of the pinned-post entry (carries
    /// `ReasonPin`). Only populated for the Posts tab; rendered with a
    /// "Pinned" badge above the row.
    pinned_idx: Option<usize>,
    next_cursor: Option<String>,
    row_heights: Vec<i32>,
    selected_idx: usize,
    fetching: bool,
    dispatched: bool,
    error: Option<String>,
}

/// Per-tab state for the Feeds tab — custom feed generators created
/// by the actor. Rows are fixed-height (`NON_POST_ROW_H`) so no lazy
/// measurement is needed.
#[derive(Default)]
struct FeedsTabState {
    items: Vec<GeneratorView>,
    next_cursor: Option<String>,
    selected_idx: usize,
    fetching: bool,
    dispatched: bool,
    error: Option<String>,
}

/// Per-tab state for the Lists tab.
#[derive(Default)]
struct ListsTabState {
    items: Vec<ListView>,
    next_cursor: Option<String>,
    selected_idx: usize,
    fetching: bool,
    dispatched: bool,
    error: Option<String>,
}

/// Per-tab state for the Starter Packs tab.
#[derive(Default)]
struct PacksTabState {
    items: Vec<StarterPackViewBasic>,
    next_cursor: Option<String>,
    selected_idx: usize,
    fetching: bool,
    dispatched: bool,
    error: Option<String>,
}

/// Fixed row height for the Feeds / Lists / Packs tabs.
const NON_POST_ROW_H: i32 = 80;
/// Avatar dimension within Feeds / Lists / Packs rows.
const NON_POST_AVATAR_SIZE: i32 = 48;

enum ProfileState {
    /// Initial state — waiting on the worker to return getProfile.
    Pending,
    /// `getProfile` returned successfully.
    Loaded(Box<ProfileViewDetailedData>),
    /// `getProfile` failed.
    Error(String),
}

pub struct ProfileScreen {
    client: Arc<AuthClient>,
    /// `None` ⇒ logged-in user's own profile (worker resolves DID from
    /// the session). `Some(handle_or_did)` ⇒ render that actor's
    /// profile.
    actor: Option<String>,
    state: ProfileState,
    /// Tracks whether we've already sent the `FetchProfile` request
    /// this session. Without this we'd re-dispatch every frame while
    /// the response is in flight.
    dispatched: bool,
    /// Avatar URL we've dispatched a fetch for; suppresses re-dispatch
    /// while in flight. Cleared on `WorkResponse::Image`.
    inflight_avatar: Option<String>,
    /// Banner URL we've dispatched a fetch for. Same suppress-while-
    /// in-flight pattern as avatar.
    inflight_banner: Option<String>,
    /// Tab bar (only rendered for the own-profile / top-level instance;
    /// pushed-sub-screen instances with `actor: Some(_)` skip rendering
    /// it because they're below the tab bar in the navigation stack).
    tab_bar: TabBar,
    /// Tap state for the Follow / Unfollow button (rendered only when
    /// `actor.is_some()`, i.e. viewing somebody else's profile).
    follow_btn: ButtonState,
    /// Tap state for the "Message" (open DM) button on other profiles.
    message_btn: ButtonState,
    /// True while a `GetConvoForMembers` is in flight (disables the
    /// Message button so a double-tap doesn't open two conversations).
    messaging: bool,
    /// Error from the last DM-open attempt (e.g. a `DM_SCOPE:` scope
    /// failure), shown briefly under the action buttons.
    dm_error: Option<String>,
    /// Set when a conversation is ready to open; consumed at the top of
    /// the next `frame()` as a `Push` (navigation can't happen inside
    /// `handle_worker_response`).
    pending_open_convo: Option<ConvoView>,
    /// Tap state for the own-profile "Log out" button.
    logout_btn: ButtonState,
    /// When `Some`, the Log out button is "armed" (awaiting a confirming
    /// second tap). Auto-disarms after a few seconds.
    logout_armed_at: Option<Instant>,
    /// Active sub-tab. Default `Posts`.
    active_tab: ProfileTab,
    /// Tap state for each pill in the sub-tab strip (parallel to
    /// `ProfileTab::ALL`).
    tab_pill_btns: [ButtonState; 7],
    /// Vertical scroll offset (pixels). When >= `TAB_STRIP_Y`, the
    /// pill strip pins to the top and tab content scrolls under it.
    scroll_y: f32,
    /// DID of the actor whose profile is being shown. Populated from
    /// the `Profile` response's `did` field. Per-tab fetches gate on
    /// this being set (until profile loads we don't know which actor
    /// to fetch for). Also acts as the staleness key for tab
    /// responses.
    resolved_did: Option<String>,
    /// Per-tab state for the four post-shaped tabs. Each tab keeps its
    /// own scroll / selection / cursor so re-tapping is instant.
    posts_tab: TabFeedState,
    replies_tab: TabFeedState,
    media_tab: TabFeedState,
    likes_tab: TabFeedState,
    /// Per-tab state for the Feeds tab (custom feed generators).
    feeds_tab: FeedsTabState,
    /// Per-tab state for the Lists tab (curated lists).
    lists_tab: ListsTabState,
    /// Per-tab state for the Packs tab (starter packs).
    packs_tab: PacksTabState,
    /// When true, the header banner is shown full-screen (aspect-fit)
    /// as a modal overlay; CIRCLE or a tap dismisses it.
    viewing_banner: bool,
    /// Clean-tap edge state for opening the banner (down-in-banner then
    /// release) — mirrors `widget::button`'s press/release detection so
    /// the same touch that opens the overlay can't also dismiss it.
    banner_open_btn: ButtonState,
    /// Clean-tap edge state for dismissing the full-screen banner.
    banner_close_btn: ButtonState,
}

impl ProfileScreen {
    /// Construct a ProfileScreen for `actor`. `None` ⇒ own profile;
    /// `Some(handle_or_did)` ⇒ that actor.
    pub fn new(client: Arc<AuthClient>, actor: Option<String>) -> Self {
        Self {
            client,
            actor,
            state: ProfileState::Pending,
            dispatched: false,
            inflight_avatar: None,
            inflight_banner: None,
            tab_bar: TabBar::new(TopLevel::Profile),
            follow_btn: ButtonState::default(),
            message_btn: ButtonState::default(),
            messaging: false,
            dm_error: None,
            pending_open_convo: None,
            logout_btn: ButtonState::default(),
            logout_armed_at: None,
            active_tab: ProfileTab::Posts,
            tab_pill_btns: Default::default(),
            scroll_y: 0.0,
            resolved_did: None,
            posts_tab: TabFeedState::default(),
            replies_tab: TabFeedState::default(),
            media_tab: TabFeedState::default(),
            likes_tab: TabFeedState::default(),
            feeds_tab: FeedsTabState::default(),
            lists_tab: ListsTabState::default(),
            packs_tab: PacksTabState::default(),
            viewing_banner: false,
            banner_open_btn: ButtonState::default(),
            banner_close_btn: ButtonState::default(),
        }
    }

    /// True if this is the user's own-profile (top-level) instance.
    fn is_own(&self) -> bool {
        self.actor.is_none()
    }

    /// Render the banner full-screen (aspect-fit on a black backdrop)
    /// as a modal overlay. Dismissed by CIRCLE or a clean tap anywhere.
    /// Returns `ScreenAction::None` always — the overlay never navigates.
    fn draw_banner_fullscreen(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
    ) -> ScreenAction {
        frame.fill_rect(
            0.0,
            0.0,
            SCREEN_WIDTH as f32,
            SCREEN_HEIGHT as f32,
            Color::rgb(0x00, 0x00, 0x00),
        );
        if let ProfileState::Loaded(p) = &self.state {
            if let Some(url) = p.banner.as_deref().map(banner_jpeg) {
                if let Some(tex) = ctx.texture_cache.get(&url) {
                    let tw = tex.width().max(1) as f32;
                    let th = tex.height().max(1) as f32;
                    let scale =
                        (SCREEN_WIDTH as f32 / tw).min(SCREEN_HEIGHT as f32 / th);
                    let dw = tw * scale;
                    let dh = th * scale;
                    let x = (SCREEN_WIDTH as f32 - dw) / 2.0;
                    let y = (SCREEN_HEIGHT as f32 - dh) / 2.0;
                    frame.draw_texture_scale(tex, x, y, scale, scale);
                }
            }
        }
        frame.draw_text_centered(
            font,
            SCREEN_HEIGHT - 22,
            theme::TEXT_MUTED,
            0.8,
            "\u{25CB} Back",
        );

        // Dismiss: CIRCLE, or a clean tap anywhere (release edge).
        let pressed_now = !ctx.touches.is_empty();
        let tapped =
            self.banner_close_btn.pressed_last && !pressed_now && ctx.touches.is_empty();
        self.banner_close_btn.pressed_last = pressed_now;
        if tapped || ctx.pad.just_pressed(buttons::CIRCLE) {
            self.viewing_banner = false;
            self.banner_close_btn.pressed_last = false;
        }
        ScreenAction::None
    }

    /// Tabs visible on this profile screen. The Likes tab is hidden on
    /// other-actor profiles because `getActorLikes` is server-side
    /// gated to the requesting account (matches the official
    /// bsky-app's behavior).
    ///
    /// FOLLOW-UP (deferred): ClearSky and similar third-party tools
    /// show anyone's likes by walking the actor's repo via
    /// `com.atproto.repo.listRecords({ collection:
    /// app.bsky.feed.like })` then hydrating the post URIs via
    /// `app.bsky.feed.getPosts` (max 25 URIs per call). When that's
    /// implemented, replace this gating so the Likes tab is always
    /// visible and dispatch routes through the new request/response
    /// path.
    fn available_tabs(&self) -> &'static [ProfileTab] {
        if self.is_own() {
            &ProfileTab::ALL
        } else {
            // Same as ALL minus Likes.
            &[
                ProfileTab::Posts,
                ProfileTab::Replies,
                ProfileTab::Media,
                ProfileTab::Feeds,
                ProfileTab::Lists,
                ProfileTab::Packs,
            ]
        }
    }

    /// Dispatch the active tab's first-page fetch if we have the
    /// resolved DID and the tab hasn't been dispatched yet. Idempotent
    /// per-tab — re-tapping a previously-loaded tab is instant.
    fn maybe_dispatch_active_tab(&mut self, ctx: &UiCtx) {
        let Some(did) = self.resolved_did.clone() else { return };
        let Some(worker) = ctx.worker else { return };
        let (state, source) = match self.active_tab {
            ProfileTab::Posts => (
                &mut self.posts_tab,
                FeedSource::AuthorPosts { actor: did },
            ),
            ProfileTab::Replies => (
                &mut self.replies_tab,
                FeedSource::AuthorReplies { actor: did },
            ),
            ProfileTab::Media => (
                &mut self.media_tab,
                FeedSource::AuthorMedia { actor: did },
            ),
            ProfileTab::Likes => (
                &mut self.likes_tab,
                FeedSource::AuthorLikes { actor: did },
            ),
            ProfileTab::Feeds => {
                if !self.feeds_tab.dispatched {
                    worker.send(WorkRequest::FetchActorFeeds {
                        actor: did,
                        cursor: None,
                    });
                    self.feeds_tab.dispatched = true;
                    self.feeds_tab.fetching = true;
                }
                return;
            }
            ProfileTab::Lists => {
                if !self.lists_tab.dispatched {
                    worker.send(WorkRequest::FetchActorLists {
                        actor: did,
                        cursor: None,
                    });
                    self.lists_tab.dispatched = true;
                    self.lists_tab.fetching = true;
                }
                return;
            }
            ProfileTab::Packs => {
                if !self.packs_tab.dispatched {
                    worker.send(WorkRequest::FetchActorStarterPacks {
                        actor: did,
                        cursor: None,
                    });
                    self.packs_tab.dispatched = true;
                    self.packs_tab.fetching = true;
                }
                return;
            }
        };
        if !state.dispatched {
            worker.send(WorkRequest::FetchFeed { source, cursor: None });
            state.dispatched = true;
            state.fetching = true;
        }
    }

    /// Active tab's post list (post-shaped tabs only). Returns None for
    /// non-post tabs (Feeds/Lists/Packs) — engagement / reply / d-pad
    /// selection no-op there.
    fn active_post_state_mut(&mut self) -> Option<&mut TabFeedState> {
        match self.active_tab {
            ProfileTab::Posts => Some(&mut self.posts_tab),
            ProfileTab::Replies => Some(&mut self.replies_tab),
            ProfileTab::Media => Some(&mut self.media_tab),
            ProfileTab::Likes => Some(&mut self.likes_tab),
            ProfileTab::Feeds | ProfileTab::Lists | ProfileTab::Packs => None,
        }
    }

    fn active_post_state(&self) -> Option<&TabFeedState> {
        match self.active_tab {
            ProfileTab::Posts => Some(&self.posts_tab),
            ProfileTab::Replies => Some(&self.replies_tab),
            ProfileTab::Media => Some(&self.media_tab),
            ProfileTab::Likes => Some(&self.likes_tab),
            ProfileTab::Feeds | ProfileTab::Lists | ProfileTab::Packs => None,
        }
    }

    /// Y at which the bottom of the (possibly-pinned) pill strip
    /// renders for a given scroll position. Used as the top edge of
    /// the visible content band.
    fn pill_bottom_y(scroll_y: f32) -> i32 {
        let pill_y = (TAB_STRIP_Y - scroll_y as i32).max(0);
        pill_y + TAB_STRIP_H
    }

    fn viewport_bottom(&self) -> i32 {
        if self.is_own() { OWN_VIEWPORT_BOTTOM } else { SCREEN_HEIGHT }
    }

    /// Stable (scroll-invariant) top y of post `i` in the active tab's
    /// row stack. Computes from the cumulative heights below the strip.
    fn stable_row_top(state: &TabFeedState, i: usize) -> i32 {
        (TAB_STRIP_Y + TAB_STRIP_H)
            + state.row_heights[..i.min(state.row_heights.len())].iter().sum::<i32>()
    }

    /// True if the current selection's row intersects the visible
    /// content band (between the pill strip's bottom and the viewport
    /// bottom). False if the analog stick has scrolled the row away.
    fn is_selected_row_visible(&self) -> bool {
        let Some(state) = self.active_post_state() else { return true };
        if state.selected_idx >= state.row_heights.len() {
            return true;
        }
        let stable_top = Self::stable_row_top(state, state.selected_idx);
        let row_h = state.row_heights[state.selected_idx];
        let screen_top = stable_top - self.scroll_y as i32;
        let screen_bot = screen_top + row_h;
        let pb = Self::pill_bottom_y(self.scroll_y);
        let vb = self.viewport_bottom();
        screen_bot > pb && screen_top < vb
    }

    /// First visible post idx for the active tab at current scroll —
    /// used to snap selection when the user has scrolled away from
    /// the prior selection and then presses d-pad.
    fn first_visible_post_idx(&self) -> usize {
        let Some(state) = self.active_post_state() else { return 0 };
        let pb = Self::pill_bottom_y(self.scroll_y);
        let vb = self.viewport_bottom();
        let mut stable_top = TAB_STRIP_Y + TAB_STRIP_H;
        for (i, &h) in state.row_heights.iter().enumerate() {
            let screen_top = stable_top - self.scroll_y as i32;
            let screen_bot = screen_top + h;
            if screen_bot > pb && screen_top < vb {
                return i;
            }
            stable_top += h;
        }
        0
    }

    /// Adjust `scroll_y` so the selected row sits inside the visible
    /// content band with at least `SCROLL_MARGIN` of breathing room
    /// at the closer edge. Mirror of TimelineScreen's auto-scroll.
    fn auto_scroll_to_selected(&mut self) {
        const SCROLL_MARGIN: i32 = 50;
        let (stable_top, row_h) = {
            let Some(state) = self.active_post_state() else { return };
            if state.selected_idx >= state.row_heights.len() {
                return;
            }
            let st = Self::stable_row_top(state, state.selected_idx);
            (st, state.row_heights[state.selected_idx])
        };
        let pb_pinned = TAB_STRIP_H; // pinned-pill case (worst-case)
        let vb = self.viewport_bottom();
        let visible_h = vb - pb_pinned;
        let screen_top = stable_top - self.scroll_y as i32;
        let screen_bot = screen_top + row_h;
        if row_h > visible_h {
            // Row is taller than the visible band — pin its top so the
            // user can read top-down.
            self.scroll_y = (stable_top - pb_pinned - SCROLL_MARGIN).max(0) as f32;
        } else if screen_top < pb_pinned + SCROLL_MARGIN {
            self.scroll_y = (stable_top - pb_pinned - SCROLL_MARGIN).max(0) as f32;
        } else if screen_bot > vb - SCROLL_MARGIN {
            self.scroll_y = (stable_top + row_h - vb + SCROLL_MARGIN).max(0) as f32;
        }
    }

    /// Toggle like/repost on the currently-focused post in the active
    /// tab. No-op if the active tab has no posts.
    fn toggle_focused_engagement(&mut self, ctx: &UiCtx, kind: EngagementKind) {
        let Some(worker) = ctx.worker else { return };
        let state = match self.active_post_state_mut() {
            Some(s) => s,
            None => return,
        };
        let Some(post) = state.posts.get_mut(state.selected_idx) else { return };
        toggle_engagement(post, worker, kind);
    }

    /// Build a `ReplyTarget` for the currently-focused post (if any).
    fn focused_reply_target(&self) -> Option<ReplyTarget> {
        let state = self.active_post_state()?;
        let post = state.posts.get(state.selected_idx)?;
        let uri = post.post.uri.clone();
        let cid = post.post.cid.as_ref().to_string();
        Some(ReplyTarget {
            parent_uri: uri.clone(),
            parent_cid: cid.clone(),
            root_uri: uri,
            root_cid: cid,
        })
    }

    /// Handle a `WorkResponse::FeedPage` — route to the matching
    /// per-tab state if the source identifies one of our author tabs
    /// AND the actor matches our resolved DID. Stale responses (e.g.
    /// from a previous profile if the user navigated away mid-fetch)
    /// are silently dropped.
    fn handle_feed_page(
        &mut self,
        source: FeedSource,
        batch: Result<bsky_worker::TimelineBatch, String>,
    ) {
        // Extract (tab, actor) from the source.
        let (tab, actor) = match &source {
            FeedSource::AuthorPosts { actor } => (ProfileTab::Posts, actor.clone()),
            FeedSource::AuthorReplies { actor } => (ProfileTab::Replies, actor.clone()),
            FeedSource::AuthorMedia { actor } => (ProfileTab::Media, actor.clone()),
            FeedSource::AuthorLikes { actor } => (ProfileTab::Likes, actor.clone()),
            // Following / Feed responses belong to TimelineScreen.
            _ => return,
        };
        // Stale: actor doesn't match our resolved DID.
        if self.resolved_did.as_deref() != Some(actor.as_str()) {
            return;
        }
        let state = match tab {
            ProfileTab::Posts => &mut self.posts_tab,
            ProfileTab::Replies => &mut self.replies_tab,
            ProfileTab::Media => &mut self.media_tab,
            ProfileTab::Likes => &mut self.likes_tab,
            ProfileTab::Feeds | ProfileTab::Lists | ProfileTab::Packs => return,
        };
        state.fetching = false;
        match batch {
            Ok(b) => {
                let was_first_page = state.posts.is_empty();
                let prev_len = state.posts.len();
                state.posts.extend(b.posts);
                state.next_cursor = b.cursor;
                state.error = None;
                // Detect pinned-post entry on the first page (only the
                // Posts tab requests include_pins). Walk the just-
                // appended slice for a `ReasonPin`.
                if was_first_page && tab == ProfileTab::Posts {
                    for (rel_i, post) in state.posts[prev_len..].iter().enumerate() {
                        if matches!(
                            post.reason.as_ref(),
                            Some(Union::Refs(FeedViewPostReasonRefs::ReasonPin(_)))
                        ) {
                            state.pinned_idx = Some(prev_len + rel_i);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                // Likes can be server-side-private for users that
                // haven't opted in: the PDS returns "Profile not found"
                // 400 when the requester isn't authorized. Surface a
                // friendlier note instead of the raw XRPC string.
                let pretty = if tab == ProfileTab::Likes && e.contains("Profile not found") {
                    "This user's likes aren't visible.".to_string()
                } else {
                    e
                };
                state.error = Some(pretty);
            }
        }
    }

    /// Optimistically toggle the follow state of the displayed actor +
    /// dispatch the CreateFollow / DeleteFollow worker request. Local
    /// state updates IMMEDIATELY so the button label flips on tap; the
    /// real URI replaces our `PENDING_URI` sentinel on response.
    fn toggle_follow(&mut self, ctx: &UiCtx) {
        let Some(worker) = ctx.worker else { return };
        let ProfileState::Loaded(p) = &mut self.state else { return };
        // Always need the target's DID — server requires the resolved
        // identifier; the user might have entered a handle but the
        // profile response carries the resolved DID.
        let actor_did = p.did.to_string();
        let viewer = match p.viewer.as_mut() {
            Some(v) => v,
            None => {
                use atrium_api::app::bsky::actor::defs::ViewerStateData;
                p.viewer = Some(
                    ViewerStateData {
                        activity_subscription: None,
                        blocked_by: None,
                        blocking: None,
                        blocking_by_list: None,
                        followed_by: None,
                        following: None,
                        known_followers: None,
                        muted: None,
                        muted_by_list: None,
                    }
                    .into(),
                );
                p.viewer.as_mut().expect("just initialized")
            }
        };
        if let Some(existing) = viewer.following.take() {
            if existing == crate::timeline::PENDING_URI {
                // Optimistic create still in flight — drop the unfollow.
                return;
            }
            let Some(rkey) = existing.rsplit('/').next().map(String::from) else {
                viewer.following = Some(existing);
                return;
            };
            worker.send(WorkRequest::DeleteFollow { rkey });
        } else {
            viewer.following = Some(crate::timeline::PENDING_URI.to_string());
            worker.send(WorkRequest::CreateFollow { actor_did });
        }
    }
}

impl Screen for ProfileScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        _ime: &mut Ime,
    ) -> ScreenAction {
        // A DM conversation became ready (from the "Message" button) —
        // open it. Done here because navigation can't happen inside
        // `handle_worker_response`.
        if let Some(convo) = self.pending_open_convo.take() {
            return ScreenAction::Push(Box::new(ConversationScreen::new(
                Arc::clone(&self.client),
                convo,
            )));
        }

        // Dispatch the fetch on the first frame. The worker is guaranteed
        // to exist by the AuthComplete invariant (main.rs spawns it before
        // pushing this screen). If it's somehow missing, fall through to
        // a static "Loading…" — the user sees a stuck screen instead of
        // a panic.
        if !self.dispatched {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::FetchProfile {
                    actor: self.actor.clone(),
                });
                self.dispatched = true;
            }
        }

        // Dispatch avatar + banner fetches if we have URLs that aren't
        // cached / already in flight. Transform avatar URL to the
        // small JPEG thumbnail so the cache lookup matches the dispatch
        // URL; banners use the full-size CDN URL with @jpeg coercion.
        if let ProfileState::Loaded(p) = &self.state {
            if let Some(url) = p.avatar.as_deref().map(avatar_thumbnail_jpeg) {
                if !ctx.texture_cache.contains(&url)
                    && self.inflight_avatar.as_deref() != Some(url.as_str())
                {
                    if let Some(worker) = ctx.worker {
                        worker.send(WorkRequest::FetchImage { url: url.clone() });
                        self.inflight_avatar = Some(url);
                    }
                }
            }
            if let Some(url) = p.banner.as_deref().map(banner_jpeg) {
                if !ctx.texture_cache.contains(&url)
                    && self.inflight_banner.as_deref() != Some(url.as_str())
                {
                    if let Some(worker) = ctx.worker {
                        worker.send(WorkRequest::FetchImage { url: url.clone() });
                        self.inflight_banner = Some(url);
                    }
                }
            }
        }

        // Full-screen banner overlay short-circuits all other render +
        // input for this frame.
        if self.viewing_banner {
            return self.draw_banner_fullscreen(frame, font, ctx);
        }

        // Banner tap → open the full-screen view. Fires on touch release
        // inside the (visible) banner strip, gated on a real banner
        // texture being cached. Release-edge detection keeps the opening
        // touch from leaking into the overlay's dismiss handler.
        if let ProfileState::Loaded(p) = &self.state {
            let has_banner = p
                .banner
                .as_deref()
                .map(banner_jpeg)
                .map(|u| ctx.texture_cache.contains(&u))
                .unwrap_or(false);
            let banner_top = -(self.scroll_y as i32);
            let banner_bottom = banner_top + BANNER_H;
            let in_banner = has_banner
                && banner_bottom > 0
                && ctx.touches.iter().any(|t| {
                    t.x >= 0
                        && t.x < SCREEN_WIDTH
                        && t.y >= banner_top.max(0)
                        && t.y < banner_bottom
                });
            let tapped = self.banner_open_btn.pressed_last
                && !in_banner
                && ctx.touches.is_empty();
            self.banner_open_btn.pressed_last = in_banner;
            if tapped {
                self.viewing_banner = true;
                self.banner_open_btn.pressed_last = false;
                return ScreenAction::None;
            }
        }

        // ─── Input ──────────────────────────────────────────────────
        // Analog-stick free scroll (mirrors TimelineScreen). Pushing
        // up = decrease scroll_y (move toward top of header).
        let stick_y = ctx.pad.left_stick.1;
        let mag = stick_y.unsigned_abs() as f32;
        let dz = STICK_DEADZONE as f32;
        if mag > dz {
            let sign: f32 = if stick_y < 0 { -1.0 } else { 1.0 };
            let effective = (mag - dz) * sign;
            self.scroll_y += effective / STICK_DIVISOR;
        }
        // D-pad LEFT/RIGHT: cycle active sub-tab. Wraps at the ends.
        // Cycling honors `available_tabs()` so hidden tabs (e.g. Likes
        // on other-actor profiles) aren't reachable.
        if matches!(&self.state, ProfileState::Loaded(_)) {
            let avail = self.available_tabs();
            if ctx.pad.just_pressed(buttons::LEFT) {
                if let Some(cur) = avail.iter().position(|&t| t == self.active_tab) {
                    let n = avail.len();
                    self.active_tab = avail[(cur + n - 1) % n];
                }
            }
            if ctx.pad.just_pressed(buttons::RIGHT) {
                if let Some(cur) = avail.iter().position(|&t| t == self.active_tab) {
                    self.active_tab = avail[(cur + 1) % avail.len()];
                }
            }
            // D-pad UP/DOWN: post selection within the active post-tab.
            // Eagerly lazy-measure row heights here so visibility checks
            // and auto-scroll have current data BEFORE the header
            // renders (which uses the post-auto-scroll y_offset).
            if let Some(state) = self.active_post_state_mut() {
                while state.row_heights.len() < state.posts.len() {
                    let i = state.row_heights.len();
                    let h = crate::timeline::measure_post_row(
                        frame,
                        font,
                        &state.posts[i],
                        ctx.emoji,
                    );
                    state.row_heights.push(h);
                }
            }
            let mut selection_changed = false;
            let dpad_pressed = ctx.pad.just_pressed(buttons::UP)
                || ctx.pad.just_pressed(buttons::DOWN);
            if dpad_pressed && self.active_post_state().is_some() {
                let n = self.active_post_state().map(|s| s.posts.len()).unwrap_or(0);
                if n > 0 {
                    // If the prior selection has scrolled off-screen
                    // (analog-stick paged the viewport away), snap
                    // selection to the first visible row BEFORE
                    // applying the d-pad direction. Avoids a visible
                    // jump back to the off-screen selection.
                    if !self.is_selected_row_visible() {
                        let snap = self.first_visible_post_idx();
                        if let Some(s) = self.active_post_state_mut() {
                            s.selected_idx = snap;
                        }
                        selection_changed = true;
                    }
                    if ctx.pad.just_pressed(buttons::UP) {
                        if let Some(s) = self.active_post_state_mut() {
                            if s.selected_idx > 0 {
                                s.selected_idx -= 1;
                                selection_changed = true;
                            }
                        }
                    }
                    if ctx.pad.just_pressed(buttons::DOWN) {
                        if let Some(s) = self.active_post_state_mut() {
                            if s.selected_idx + 1 < s.posts.len() {
                                s.selected_idx += 1;
                                selection_changed = true;
                            }
                        }
                    }
                }
            }
            if selection_changed {
                self.auto_scroll_to_selected();
            }
            // D-pad UP/DOWN on the non-post tabs (Feeds / Lists /
            // Packs) — fixed-height rows so no measurement gymnastics
            // needed; no auto-scroll for v1.
            match self.active_tab {
                ProfileTab::Feeds => {
                    let n = self.feeds_tab.items.len();
                    if n > 0 {
                        if ctx.pad.just_pressed(buttons::UP)
                            && self.feeds_tab.selected_idx > 0
                        {
                            self.feeds_tab.selected_idx -= 1;
                        }
                        if ctx.pad.just_pressed(buttons::DOWN)
                            && self.feeds_tab.selected_idx + 1 < n
                        {
                            self.feeds_tab.selected_idx += 1;
                        }
                    }
                }
                ProfileTab::Lists => {
                    let n = self.lists_tab.items.len();
                    if n > 0 {
                        if ctx.pad.just_pressed(buttons::UP)
                            && self.lists_tab.selected_idx > 0
                        {
                            self.lists_tab.selected_idx -= 1;
                        }
                        if ctx.pad.just_pressed(buttons::DOWN)
                            && self.lists_tab.selected_idx + 1 < n
                        {
                            self.lists_tab.selected_idx += 1;
                        }
                    }
                }
                ProfileTab::Packs => {
                    let n = self.packs_tab.items.len();
                    if n > 0 {
                        if ctx.pad.just_pressed(buttons::UP)
                            && self.packs_tab.selected_idx > 0
                        {
                            self.packs_tab.selected_idx -= 1;
                        }
                        if ctx.pad.just_pressed(buttons::DOWN)
                            && self.packs_tab.selected_idx + 1 < n
                        {
                            self.packs_tab.selected_idx += 1;
                        }
                    }
                }
                _ => {}
            }
            // L1 = like, TRIANGLE = repost on the focused post.
            if ctx.pad.just_pressed(buttons::L1) {
                self.toggle_focused_engagement(ctx, EngagementKind::Like);
            }
            if ctx.pad.just_pressed(buttons::TRIANGLE) {
                self.toggle_focused_engagement(ctx, EngagementKind::Repost);
            }
            // R1 = reply: push ComposeScreen prefilled with the
            // focused post as parent.
            if ctx.pad.just_pressed(buttons::R1) {
                if let Some(target) = self.focused_reply_target() {
                    let handle = self
                        .active_post_state()
                        .and_then(|s| s.posts.get(s.selected_idx))
                        .map(|p| p.post.author.handle.as_str().to_string());
                    return ScreenAction::Push(Box::new(ComposeScreen::new(
                        Arc::clone(&self.client),
                        Some(target),
                        handle,
                    )));
                }
            }
        }
        // Lower-bound the scroll. The upper bound is recomputed below
        // once we know the active tab's content height.
        if self.scroll_y < 0.0 {
            self.scroll_y = 0.0;
        }
        let y_offset: i32 = -(self.scroll_y as i32);

        let mut toggle_follow_clicked = false;
        // DID to open a DM with, captured when the Message button is
        // clicked (so the dispatch happens after the `self.state` borrow
        // ends below).
        let mut open_dm_did: Option<String> = None;
        // Set when the (own-profile) Log out button's confirming tap fires.
        let mut logout_clicked = false;
        match &self.state {
            ProfileState::Pending => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2,
                    theme::TEXT_MUTED,
                    1.1,
                    "Loading profile…",
                );
            }
            ProfileState::Loaded(p) => {
                draw_profile(frame, font, p, ctx.texture_cache, ctx.avatar_mask, y_offset);
                if !self.is_own() {
                    // Follow / Unfollow + Message buttons on other actors'
                    // profiles, side by side. Sits in the action-row band
                    // between the meta line (y=325) and the pill strip
                    // (y=374).
                    let following = p
                        .viewer
                        .as_ref()
                        .and_then(|v| v.following.as_deref())
                        .is_some();
                    let label = if following { "Unfollow" } else { "Follow" };
                    let btn_w = 150.0;
                    let gap = 12.0;
                    let total = btn_w * 2.0 + gap;
                    let left_x = (SCREEN_WIDTH as f32 - total) / 2.0;
                    let btn_y = 340.0 + y_offset as f32;
                    let follow_rect = Rect::new(left_x, btn_y, btn_w, 30.0);
                    if button(frame, font, follow_rect, label, &mut self.follow_btn, ctx, true) {
                        toggle_follow_clicked = true;
                    }
                    let msg_rect = Rect::new(left_x + btn_w + gap, btn_y, btn_w, 30.0);
                    let msg_label = if self.messaging { "Opening…" } else { "Message" };
                    if button(
                        frame,
                        font,
                        msg_rect,
                        msg_label,
                        &mut self.message_btn,
                        ctx,
                        !self.messaging,
                    ) {
                        open_dm_did = Some(p.did.as_str().to_string());
                    }
                    // The DM-open error is drawn as a bottom toast at the
                    // end of frame() (the action band here is too cramped —
                    // the sub-tab pill strip at y=374 would cover it).
                } else {
                    // Own profile: Log out button in the same action band.
                    // Two-tap confirm (armed for ~3s) so an accidental tap
                    // can't sign the user out (re-entering an app password
                    // on the IME is painful). Drawn inline rather than via
                    // `widget::button` so the armed state can go red.
                    let armed = self
                        .logout_armed_at
                        .map(|t| t.elapsed() < Duration::from_secs(3))
                        .unwrap_or(false);
                    let btn_w = 160.0;
                    let rect = Rect::new(
                        (SCREEN_WIDTH as f32 - btn_w) / 2.0,
                        340.0 + y_offset as f32,
                        btn_w,
                        30.0,
                    );
                    let (label, color) = if armed {
                        ("Confirm log out?", theme::ERROR)
                    } else {
                        ("Log out", theme::ACCENT)
                    };
                    let pressed_now = ctx.touches.iter().any(|t| rect.contains(t.x, t.y));
                    let clicked = self.logout_btn.pressed_last
                        && !pressed_now
                        && ctx.touches.is_empty();
                    self.logout_btn.pressed_last = pressed_now;
                    frame.fill_rect(rect.x, rect.y, rect.w, rect.h, color);
                    let scale = 1.1;
                    let (tw, th) = frame.measure_text(font, scale, label);
                    let tx = rect.x as i32 + (rect.w as i32 - tw) / 2;
                    let ty = rect.y as i32 + (rect.h as i32 + th) / 2 - 4;
                    frame.draw_text(font, tx, ty, theme::TEXT_PRIMARY, scale, label);
                    if clicked {
                        if armed {
                            logout_clicked = true;
                        } else {
                            self.logout_armed_at = Some(Instant::now());
                        }
                    }
                }
            }
            ProfileState::Error(msg) => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 - 20,
                    theme::ERROR,
                    1.0,
                    "Could not load profile",
                );
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 + 20,
                    theme::TEXT_MUTED,
                    0.85,
                    msg,
                );
            }
        }

        // Apply Follow / Unfollow toggle outside the match so the
        // borrow on `self.state` ends first.
        if toggle_follow_clicked {
            self.toggle_follow(ctx);
        }

        // Open-DM: dispatch getConvoForMembers for the tapped actor. The
        // response arrives as `ConvoForMembers`, which stashes the convo
        // in `pending_open_convo` for the next frame to push.
        if let Some(did) = open_dm_did {
            if !self.messaging {
                if let Some(worker) = ctx.worker {
                    self.dm_error = None;
                    self.messaging = true;
                    worker.send(WorkRequest::GetConvoForMembers {
                        members: vec![did],
                    });
                }
            }
        }

        // Log out: confirming tap fired — main.rs tears down the session.
        if logout_clicked {
            return ScreenAction::Logout;
        }

        // ─── Tab content + sub-tab pill strip ───────────────────────
        // Render tab content FIRST so the pill strip can overdraw any
        // rows that scrolled into the strip's pinned area at y=0..40.
        if matches!(&self.state, ProfileState::Loaded(_)) {
            // Dispatch active-tab fetch on first activation (per-tab),
            // gated on having the resolved DID from the profile response.
            self.maybe_dispatch_active_tab(ctx);

            // Row heights for the active post-tab were lazy-measured up
            // top (so D-pad auto-scroll could use them); no measurement
            // needed here.

            // Compute scroll bounds based on active tab's content height.
            let viewport_bottom = if self.is_own() {
                OWN_VIEWPORT_BOTTOM
            } else {
                SCREEN_HEIGHT
            };
            let total_h: i32 = match self.active_tab {
                ProfileTab::Posts => self.posts_tab.row_heights.iter().sum(),
                ProfileTab::Replies => self.replies_tab.row_heights.iter().sum(),
                ProfileTab::Media => self.media_tab.row_heights.iter().sum(),
                ProfileTab::Likes => self.likes_tab.row_heights.iter().sum(),
                ProfileTab::Feeds => {
                    self.feeds_tab.items.len() as i32 * NON_POST_ROW_H
                }
                ProfileTab::Lists => {
                    self.lists_tab.items.len() as i32 * NON_POST_ROW_H
                }
                ProfileTab::Packs => {
                    self.packs_tab.items.len() as i32 * NON_POST_ROW_H
                }
            };
            // Max scroll: enough so the last row's bottom rests at the
            // viewport bottom. Min: TAB_STRIP_Y so the strip can always
            // pin to the top, even with sparse content.
            let content_natural_top = TAB_STRIP_Y + TAB_STRIP_H;
            let max_scroll =
                ((content_natural_top + total_h - viewport_bottom).max(TAB_STRIP_Y)) as f32;
            if self.scroll_y > max_scroll {
                self.scroll_y = max_scroll;
            }
            let y_offset: i32 = -(self.scroll_y as i32);
            let visual_strip_y = (TAB_STRIP_Y + y_offset).max(0);
            let pill_bottom = visual_strip_y + TAB_STRIP_H;

            // Pagination trigger. Same near-bottom threshold for every
            // tab variant; just the request differs.
            let near_bottom = self.scroll_y as i32 + viewport_bottom + PAGINATION_THRESHOLD
                >= content_natural_top + total_h;
            if near_bottom {
                if let (Some(worker), Some(did)) =
                    (ctx.worker, self.resolved_did.as_deref())
                {
                    let did = did.to_string();
                    let dispatch = match self.active_tab {
                        ProfileTab::Posts
                            if !self.posts_tab.fetching
                                && self.posts_tab.next_cursor.is_some() =>
                        {
                            self.posts_tab.fetching = true;
                            Some(WorkRequest::FetchFeed {
                                source: FeedSource::AuthorPosts { actor: did },
                                cursor: self.posts_tab.next_cursor.clone(),
                            })
                        }
                        ProfileTab::Replies
                            if !self.replies_tab.fetching
                                && self.replies_tab.next_cursor.is_some() =>
                        {
                            self.replies_tab.fetching = true;
                            Some(WorkRequest::FetchFeed {
                                source: FeedSource::AuthorReplies { actor: did },
                                cursor: self.replies_tab.next_cursor.clone(),
                            })
                        }
                        ProfileTab::Media
                            if !self.media_tab.fetching
                                && self.media_tab.next_cursor.is_some() =>
                        {
                            self.media_tab.fetching = true;
                            Some(WorkRequest::FetchFeed {
                                source: FeedSource::AuthorMedia { actor: did },
                                cursor: self.media_tab.next_cursor.clone(),
                            })
                        }
                        ProfileTab::Likes
                            if !self.likes_tab.fetching
                                && self.likes_tab.next_cursor.is_some() =>
                        {
                            self.likes_tab.fetching = true;
                            Some(WorkRequest::FetchFeed {
                                source: FeedSource::AuthorLikes { actor: did },
                                cursor: self.likes_tab.next_cursor.clone(),
                            })
                        }
                        ProfileTab::Feeds
                            if !self.feeds_tab.fetching
                                && self.feeds_tab.next_cursor.is_some() =>
                        {
                            self.feeds_tab.fetching = true;
                            Some(WorkRequest::FetchActorFeeds {
                                actor: did,
                                cursor: self.feeds_tab.next_cursor.clone(),
                            })
                        }
                        ProfileTab::Lists
                            if !self.lists_tab.fetching
                                && self.lists_tab.next_cursor.is_some() =>
                        {
                            self.lists_tab.fetching = true;
                            Some(WorkRequest::FetchActorLists {
                                actor: did,
                                cursor: self.lists_tab.next_cursor.clone(),
                            })
                        }
                        ProfileTab::Packs
                            if !self.packs_tab.fetching
                                && self.packs_tab.next_cursor.is_some() =>
                        {
                            self.packs_tab.fetching = true;
                            Some(WorkRequest::FetchActorStarterPacks {
                                actor: did,
                                cursor: self.packs_tab.next_cursor.clone(),
                            })
                        }
                        _ => None,
                    };
                    if let Some(req) = dispatch {
                        worker.send(req);
                    }
                }
            }

            // Render content for any post-shaped tab (Posts / Replies
            // / Media / Likes) — same code path. Non-post tabs fall to
            // a TODO placeholder. Pre-clone the client so we can hand
            // it to pushed sub-screens without borrowing self while
            // active_post_state_mut() holds a mutable borrow.
            let mut tap_screen_action: Option<ScreenAction> = None;
            let active_label = self.active_tab.label();
            let client_for_push = Arc::clone(&self.client);
            if let Some(state) = self.active_post_state_mut() {
                draw_posts_content(
                    frame,
                    font,
                    state,
                    ctx,
                    content_natural_top + y_offset,
                    pill_bottom,
                    viewport_bottom,
                );
                if !ctx.touches.is_empty() {
                    // Exclude taps inside the sticky pill strip's band (top)
                    // so they fall through to the pill hit-test (drawn last);
                    // without this a content row scrolled under the pinned
                    // strip steals taps meant for the tabs (plan Risk #1).
                    // Also exclude the bottom tab-bar band: on the own
                    // profile `viewport_bottom` is SCREEN_HEIGHT - bar, so
                    // this stops content taps from falling through the bar;
                    // on a pushed profile it's SCREEN_HEIGHT (a no-op).
                    let touches: Vec<(i32, i32)> = ctx
                        .touches
                        .iter()
                        .filter(|t| t.y >= pill_bottom && t.y < viewport_bottom)
                        .map(|t| (t.x, t.y))
                        .collect();
                    let mut row_y = content_natural_top + y_offset;
                    let mut tap: Option<TapAction> = None;
                    for (idx, post) in state.posts.iter().enumerate() {
                        let row_h = state.row_heights.get(idx).copied().unwrap_or(0);
                        let row_bottom = row_y + row_h;
                        if row_bottom > pill_bottom && row_y < viewport_bottom {
                            if let Some(t) = detect_post_tap_action(
                                frame, font, post, row_y, row_h, &touches, ctx.emoji, idx,
                            ) {
                                tap = Some(t);
                                break;
                            }
                        }
                        row_y = row_bottom;
                    }
                    match tap {
                        Some(TapAction::OpenProfile(handle)) => {
                            tap_screen_action = Some(ScreenAction::Push(Box::new(
                                ProfileScreen::new(client_for_push, Some(handle)),
                            )));
                        }
                        Some(TapAction::OpenThread(uri)) => {
                            tap_screen_action = Some(ScreenAction::Push(Box::new(
                                ThreadScreen::new(client_for_push, uri),
                            )));
                        }
                        Some(TapAction::OpenVideo(target)) => {
                            tap_screen_action = Some(ScreenAction::Push(Box::new(
                                VideoPlayerScreen::new(
                                    client_for_push,
                                    target.did,
                                    target.cid,
                                ),
                            )));
                        }
                        Some(TapAction::ToggleLike(idx)) => {
                            if let Some(worker) = ctx.worker {
                                if let Some(post) = state.posts.get_mut(idx) {
                                    toggle_engagement(post, worker, EngagementKind::Like);
                                }
                            }
                        }
                        Some(TapAction::ToggleRepost(idx)) => {
                            if let Some(worker) = ctx.worker {
                                if let Some(post) = state.posts.get_mut(idx) {
                                    toggle_engagement(post, worker, EngagementKind::Repost);
                                }
                            }
                        }
                        Some(TapAction::OpenImage { images, index }) => {
                            tap_screen_action = Some(ScreenAction::Push(Box::new(
                                crate::image_viewer::ImageViewerScreen::new(images, index),
                            )));
                        }
                        None => {}
                    }
                }
            } else if matches!(self.active_tab, ProfileTab::Feeds) {
                let content_top = content_natural_top + y_offset;
                draw_feeds_content(
                    frame,
                    font,
                    &self.feeds_tab,
                    ctx,
                    content_top,
                    pill_bottom,
                    viewport_bottom,
                );
                // Tap → push TimelineScreen of the tapped feed.
                if !ctx.touches.is_empty() {
                    // Exclude taps inside the sticky pill strip's band (top)
                    // so they fall through to the pill hit-test (drawn last);
                    // without this a content row scrolled under the pinned
                    // strip steals taps meant for the tabs (plan Risk #1).
                    // Also exclude the bottom tab-bar band: on the own
                    // profile `viewport_bottom` is SCREEN_HEIGHT - bar, so
                    // this stops content taps from falling through the bar;
                    // on a pushed profile it's SCREEN_HEIGHT (a no-op).
                    let touches: Vec<(i32, i32)> = ctx
                        .touches
                        .iter()
                        .filter(|t| t.y >= pill_bottom && t.y < viewport_bottom)
                        .map(|t| (t.x, t.y))
                        .collect();
                    let mut row_y = content_top;
                    for item in self.feeds_tab.items.iter() {
                        let row_bottom = row_y + NON_POST_ROW_H;
                        if row_bottom > pill_bottom && row_y < viewport_bottom {
                            if touches.iter().any(|&(x, y)| {
                                x >= 0
                                    && x < SCREEN_WIDTH
                                    && y >= row_y
                                    && y < row_bottom
                            }) {
                                tap_screen_action = Some(ScreenAction::Push(Box::new(
                                    crate::timeline::TimelineScreen::with_feed(
                                        client_for_push,
                                        item.uri.clone(),
                                    ),
                                )));
                                break;
                            }
                        }
                        row_y = row_bottom;
                    }
                }
            } else if matches!(self.active_tab, ProfileTab::Lists) {
                draw_lists_content(
                    frame,
                    font,
                    &self.lists_tab,
                    ctx,
                    content_natural_top + y_offset,
                    pill_bottom,
                    viewport_bottom,
                );
                // No tap action wired in v1 — no list-viewer screen yet.
            } else if matches!(self.active_tab, ProfileTab::Packs) {
                draw_packs_content(
                    frame,
                    font,
                    &self.packs_tab,
                    ctx,
                    content_natural_top + y_offset,
                    pill_bottom,
                    viewport_bottom,
                );
                // No tap action wired in v1 — no pack-viewer screen yet.
            } else {
                let label = format!("TODO: {} tab content", active_label);
                frame.draw_text_centered(
                    font,
                    pill_bottom + 30,
                    theme::TEXT_MUTED,
                    0.95,
                    &label,
                );
            }
            // If a tap led to a Push/Pop, return early (after any
            // visual state we already drew this frame).
            if let Some(action) = tap_screen_action {
                return action;
            }

            // Pill strip drawn LAST so it overdraws content scrolled
            // into the y=0..40 sticky band. Iterates `available_tabs`
            // so hidden tabs (e.g. Likes on other-actor profiles) get
            // no pill.
            let avail = self.available_tabs();
            let mut tab_clicked: Option<ProfileTab> = None;
            for (i, tab) in avail.iter().enumerate() {
                if let Some(t) = draw_tab_pill(
                    frame,
                    font,
                    *tab,
                    self.active_tab,
                    visual_strip_y,
                    i,
                    avail,
                    &mut self.tab_pill_btns[i],
                    ctx,
                ) {
                    tab_clicked = Some(t);
                }
            }
            if let Some(t) = tab_clicked {
                self.active_tab = t;
            }
        }

        // DM-open error toast — drawn LAST so the sub-tab pill strip
        // (y=374) can't cover it. Only other-actor profiles set
        // `dm_error`, and those render to the screen bottom (no tab bar),
        // so a bottom strip is unobstructed.
        if !self.is_own() {
            if let Some(err) = &self.dm_error {
                let msg = dm_error_message(err);
                let bar_y = SCREEN_HEIGHT - 34;
                frame.fill_rect(
                    0.0,
                    bar_y as f32,
                    SCREEN_WIDTH as f32,
                    34.0,
                    theme::FIELD_BG,
                );
                frame.draw_text_centered(font, bar_y + 22, theme::ERROR, 0.85, msg);
            }
        }

        // Top-level (own profile) renders the tab bar and treats CIRCLE
        // as a no-op. Pushed sub-screen (other actor's profile) skips
        // the tab bar and pops on CIRCLE.
        if self.is_own() {
            if let Some(target) = self.tab_bar.render(frame, font, ctx) {
                return ScreenAction::SwitchTab(target);
            }
        } else if ctx.pad.just_pressed(bsky_input::buttons::CIRCLE) {
            return ScreenAction::Pop;
        }

        ScreenAction::None
    }

    fn top_level(&self) -> Option<TopLevel> {
        if self.is_own() {
            Some(TopLevel::Profile)
        } else {
            None
        }
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        match resp {
            WorkResponse::Profile(Ok(p)) => {
                // Capture the resolved DID — gates per-tab fetches and
                // is the staleness key for tab responses.
                self.resolved_did = Some(p.did.to_string());
                self.state = ProfileState::Loaded(p);
            }
            WorkResponse::Profile(Err(e)) => self.state = ProfileState::Error(e),
            // FeedPage responses for our author-tab variants. Stale
            // responses (a different actor or tab from what's now
            // active) are dropped via the source-match.
            WorkResponse::FeedPage { source, batch } => {
                self.handle_feed_page(source, batch);
            }
            // Saved-feeds responses belong to TimelineScreen.
            WorkResponse::SavedFeeds(_) => {}
            // Image responses: cache is updated in main.rs; we just clear
            // our in-flight tracker so a future cache-miss can re-dispatch.
            WorkResponse::Image { url, .. } => {
                if self.inflight_avatar.as_deref() == Some(&url) {
                    self.inflight_avatar = None;
                }
                if self.inflight_banner.as_deref() == Some(&url) {
                    self.inflight_banner = None;
                }
            }
            WorkResponse::PostCreated(_) => {}
            // Resolve the in-flight PENDING_URI sentinel on any
            // post-shaped tab's post so subsequent un-like / un-repost
            // can extract the real rkey. The same post may live in
            // multiple tabs (e.g. Posts and Likes) — patch them all.
            WorkResponse::LikeChanged(Ok(Some(uri))) => {
                for state in [
                    &mut self.posts_tab,
                    &mut self.replies_tab,
                    &mut self.media_tab,
                    &mut self.likes_tab,
                ] {
                    for post in state.posts.iter_mut() {
                        if let Some(viewer) = post.post.viewer.as_mut() {
                            if viewer.like.as_deref() == Some(crate::timeline::PENDING_URI) {
                                viewer.like = Some(uri.clone());
                            }
                        }
                    }
                }
            }
            WorkResponse::RepostChanged(Ok(Some(uri))) => {
                for state in [
                    &mut self.posts_tab,
                    &mut self.replies_tab,
                    &mut self.media_tab,
                    &mut self.likes_tab,
                ] {
                    for post in state.posts.iter_mut() {
                        if let Some(viewer) = post.post.viewer.as_mut() {
                            if viewer.repost.as_deref() == Some(crate::timeline::PENDING_URI) {
                                viewer.repost = Some(uri.clone());
                            }
                        }
                    }
                }
            }
            WorkResponse::LikeChanged(_) | WorkResponse::RepostChanged(_) => {}
            WorkResponse::ActorFeeds { actor, batch } => {
                if self.resolved_did.as_deref() == Some(actor.as_str()) {
                    self.feeds_tab.fetching = false;
                    match batch {
                        Ok(b) => {
                            self.feeds_tab.items.extend(b.feeds);
                            self.feeds_tab.next_cursor = b.cursor;
                            self.feeds_tab.error = None;
                        }
                        Err(e) => self.feeds_tab.error = Some(e),
                    }
                }
            }
            WorkResponse::ActorLists { actor, batch } => {
                if self.resolved_did.as_deref() == Some(actor.as_str()) {
                    self.lists_tab.fetching = false;
                    match batch {
                        Ok(b) => {
                            self.lists_tab.items.extend(b.lists);
                            self.lists_tab.next_cursor = b.cursor;
                            self.lists_tab.error = None;
                        }
                        Err(e) => self.lists_tab.error = Some(e),
                    }
                }
            }
            WorkResponse::ActorStarterPacks { actor, batch } => {
                if self.resolved_did.as_deref() == Some(actor.as_str()) {
                    self.packs_tab.fetching = false;
                    match batch {
                        Ok(b) => {
                            self.packs_tab.items.extend(b.starter_packs);
                            self.packs_tab.next_cursor = b.cursor;
                            self.packs_tab.error = None;
                        }
                        Err(e) => self.packs_tab.error = Some(e),
                    }
                }
            }
            WorkResponse::Thread(_) => {}
            WorkResponse::FollowChanged(Ok(Some(uri))) => {
                if let ProfileState::Loaded(p) = &mut self.state {
                    if let Some(viewer) = p.viewer.as_mut() {
                        if viewer.following.as_deref() == Some(crate::timeline::PENDING_URI) {
                            viewer.following = Some(uri);
                        }
                    }
                }
            }
            WorkResponse::FollowChanged(_) => {
                // Delete acks (Ok(None)) and errors land here; no-op.
            }
            // Search results belong to SearchScreen.
            WorkResponse::SearchActors(_) | WorkResponse::SearchPosts(_) => {}
            WorkResponse::Notifications(_) => {}
            WorkResponse::VideoBlob { .. } | WorkResponse::VideoBlobProgress { .. } => {}
            // The "Message" button's getConvoForMembers result: stash the
            // convo to open on the next frame (or surface the error).
            WorkResponse::ConvoForMembers(Ok(convo)) => {
                self.messaging = false;
                self.dm_error = None;
                self.pending_open_convo = Some(convo);
            }
            WorkResponse::ConvoForMembers(Err(e)) => {
                self.messaging = false;
                self.dm_error = Some(e);
            }
            // Other DM responses belong to the conversation screens.
            WorkResponse::Convos(_)
            | WorkResponse::ConvoMessages { .. }
            | WorkResponse::MessageSent { .. }
            | WorkResponse::ConvoRead(_) => {}
        }
    }
}

/// Map a `getConvoForMembers` error into a short, actionable message
/// for the DM-open toast. Recognizes the common chat-service rejections.
fn dm_error_message(err: &str) -> &'static str {
    if err.starts_with("DM_SCOPE:") {
        "This app password can't access DMs."
    } else if err.contains("NotFollowedBySender") {
        "They only accept DMs from people they follow."
    } else if err.contains("Recipient") || err.contains("disabled") || err.contains("cannot") {
        "This user isn't accepting messages."
    } else {
        "Couldn't open conversation."
    }
}

fn draw_profile(
    frame: &mut Frame,
    font: &Font,
    p: &ProfileViewDetailedData,
    cache: &TextureCache,
    avatar_mask: Option<&Texture>,
    y_offset: i32,
) {
    // Banner strip at the very top — full-width 8:1 strip cropped from
    // whatever the source aspect is. Cache miss / no banner field falls
    // back to a solid FIELD_BG block so the avatar still has something
    // to overlap. Scrolls with `y_offset` so the user can pull the
    // banner off the top to read more content below.
    draw_banner(frame, p, cache, y_offset);

    // Avatar slot: 96×96 centered horizontally, overlapping the banner.
    // Avatar textures coming through the texture cache get a circular
    // alpha mask applied automatically (see main.rs's WorkResponse::
    // Image handler), so we can just draw them straight — the corners
    // are transparent and the banner shows through naturally. The
    // placeholder path uses fill_circle for the same effect.
    let avatar_x = (SCREEN_WIDTH - AVATAR_SIZE) / 2;
    let avatar_y = AVATAR_Y + y_offset;
    let cx = (avatar_x + AVATAR_SIZE / 2) as f32;
    let cy = (avatar_y + AVATAR_SIZE / 2) as f32;
    let mut painted_real = false;
    if let Some(url) = p.avatar.as_deref().map(avatar_thumbnail_jpeg) {
        if let Some(tex) = cache.get(&url) {
            let sx = AVATAR_SIZE as f32 / tex.width().max(1) as f32;
            let sy = AVATAR_SIZE as f32 / tex.height().max(1) as f32;
            frame.draw_texture_scale(tex, avatar_x as f32, avatar_y as f32, sx, sy);
            painted_real = true;
        }
    }
    if !painted_real {
        // Placeholder: colored CIRCLE + initial. Circle rather than
        // rect so the placeholder also looks circular against the
        // banner.
        frame.fill_circle(cx, cy, (AVATAR_SIZE / 2) as f32, placeholder_color(p.handle.as_str()));
        let source = p
            .display_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| p.handle.as_str());
        let initial = source
            .chars()
            .next()
            .unwrap_or('?')
            .to_ascii_uppercase()
            .to_string();
        let scale = AVATAR_SIZE as f32 / 40.0;
        let (tw, th) = frame.measure_text(font, scale, &initial);
        let tx = avatar_x + (AVATAR_SIZE - tw) / 2;
        let ty = avatar_y + (AVATAR_SIZE + th) / 2 - 4;
        frame.draw_text(font, tx, ty, theme::BACKGROUND, scale, &initial);
    }
    let _ = avatar_mask;  // No longer needed for the 96px avatar.

    // Display name (or fallback to handle). Baseline below the avatar
    // ring (which extends to y=180); allow an ascender's worth of
    // clearance so the text doesn't overlap the avatar. Append a
    // verified checkmark to the right of the name (ACCENT-tinted)
    // when the actor is a verified account.
    let display = p
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p.handle.as_str());
    let is_verified = p
        .verification
        .as_ref()
        .map(|v| v.verified_status == "valid")
        .unwrap_or(false);
    let (name_x, name_y, name_w, _name_h) =
        frame.draw_text_centered(font, 210 + y_offset, theme::TEXT_PRIMARY, 1.4, display);
    if is_verified {
        // Bsky uses a small filled-circle + check glyph; Inter has a
        // U+2713 ✓ (CHECK MARK) we can render in ACCENT. Visually
        // close enough for the Vita's low-DPI screen.
        frame.draw_text(font, name_x + name_w + 6, name_y, theme::ACCENT, 1.2, "✓");
    }
    let _ = name_x;

    // @handle, with pronouns appended in muted parens if present.
    let handle = match p.pronouns.as_deref().filter(|s| !s.is_empty()) {
        Some(pron) => format!("@{} ({})", p.handle.as_str(), pron),
        None => format!("@{}", p.handle.as_str()),
    };
    frame.draw_text_centered(font, 240 + y_offset, theme::TEXT_MUTED, 0.95, &handle);

    // Description (multi-line wrapped, emoji-aware). Constrained to 2
    // lines now that the header is more compressed; 2nd line ellipsises
    // on overflow.
    if let Some(desc) = p.description.as_deref().filter(|s| !s.is_empty()) {
        const DESC_MAX_W: i32 = SCREEN_WIDTH - 80;
        const DESC_MAX_LINES: usize = 2;
        let line_h = frame.measure_text(font, 0.95, "Hg").1 + 4;
        let max_h = DESC_MAX_LINES as i32 * line_h;
        // Truncate to fit the height budget by repeatedly trimming
        // characters until the wrapped text fits.
        let mut clipped = desc.to_string();
        while frame.measure_text_wrapped_with_emoji(font, DESC_MAX_W, 0.95, &clipped, None) > max_h
        {
            // Pop chars from the end until it fits; preserve trailing ellipsis.
            clipped.pop();
            // Strip a trailing ellipsis if present so we don't accumulate.
            if clipped.ends_with('…') {
                clipped.pop();
            }
            if clipped.is_empty() {
                break;
            }
        }
        let final_text = if clipped.len() < desc.len() {
            // Trim a final word boundary for cleaner ellipsis placement.
            while !clipped.is_empty() && !clipped.ends_with(char::is_whitespace) {
                clipped.pop();
            }
            format!("{}…", clipped.trim_end())
        } else {
            clipped
        };
        let desc_x = (SCREEN_WIDTH - DESC_MAX_W) / 2;
        frame.draw_text_wrapped_with_emoji(
            font,
            desc_x,
            265 + y_offset,
            DESC_MAX_W,
            theme::TEXT_PRIMARY,
            0.95,
            &final_text,
            None,
        );
    }

    // Counts row: posts | followers | following.
    let posts = p.posts_count.unwrap_or(0);
    let followers = p.followers_count.unwrap_or(0);
    let follows = p.follows_count.unwrap_or(0);
    let line = format!(
        "{posts} posts     {followers} followers     {follows} following"
    );
    frame.draw_text_centered(font, 305 + y_offset, theme::TEXT_MUTED, 0.95, &line);

    // Meta line below counts: "Joined Mar 2024 · website · joined via @x's pack"
    // Each segment is optional and " · "-separated.
    let mut segments: Vec<String> = Vec::new();
    if let Some(joined) = p.created_at.as_ref().and_then(format_month_year) {
        segments.push(format!("Joined {joined}"));
    }
    if let Some(site) = p.website.as_deref().filter(|s| !s.is_empty()) {
        // Strip a leading scheme so the line stays compact.
        let trimmed = site
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        segments.push(trimmed.to_string());
    }
    if let Some(pack) = p.joined_via_starter_pack.as_ref() {
        let creator = pack.creator.handle.as_str();
        segments.push(format!("via @{creator}'s pack"));
    }
    if !segments.is_empty() {
        let line = segments.join("  ·  ");
        frame.draw_text_centered(font, 325 + y_offset, theme::TEXT_MUTED, 0.85, &line);
    }
}

/// Format an atrium `Datetime` as `"Mon YYYY"` (e.g. "Mar 2024"). Pulls
/// the date from the RFC3339 string the type carries — no chrono dep.
/// Returns `None` if parsing fails (defensive; typed Datetime should
/// always succeed).
fn format_month_year(dt: &atrium_api::types::string::Datetime) -> Option<String> {
    let s = dt.as_str();
    // RFC3339: "2024-03-15T12:34:56.000000Z"
    let year = s.get(0..4)?;
    let month_n = s.get(5..7)?.parse::<u32>().ok()?;
    let mon = match month_n {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => return None,
    };
    Some(format!("{mon} {year}"))
}

/// Render the 960×120 banner strip at the top, shifted by `y_offset`
/// when the user has scrolled. Cache hit → texture part-scaled to fit
/// (uniform x/y scale, vertically center-cropped). Cache miss / no
/// banner → fill with FIELD_BG so the avatar still has somewhere to
/// sit.
fn draw_banner(
    frame: &mut Frame,
    p: &ProfileViewDetailedData,
    cache: &TextureCache,
    y_offset: i32,
) {
    let dst_y = y_offset as f32;
    // Background fallback first; if we have a real banner, we'll
    // overdraw it. Also covers any letterbox bands when the source's
    // aspect is wider than 8:1.
    frame.fill_rect(0.0, dst_y, SCREEN_WIDTH as f32, BANNER_H as f32, theme::FIELD_BG);

    let Some(url) = p.banner.as_deref().map(banner_jpeg) else { return };
    let Some(tex) = cache.get(&url) else { return };
    let src_w = tex.width().max(1) as f32;
    let src_h = tex.height().max(1) as f32;
    let dst_w = SCREEN_WIDTH as f32;
    let dst_h = BANNER_H as f32;
    let scale = dst_w / src_w;
    let visible_src_h = (dst_h / scale).min(src_h);
    let src_y = ((src_h - visible_src_h) / 2.0).max(0.0);
    frame.draw_texture_part_scale(
        tex,
        0.0,
        dst_y,
        0.0,
        src_y,
        src_w,
        visible_src_h,
        scale,
        scale,
    );
}

/// Render one pill in the sub-tab strip + hit-test it. Returns
/// `Some(tab)` if THIS pill was tapped (and isn't already active).
/// Pill widths are content-dependent (label + padding); positions are
/// computed by summing prior pill widths so the strip auto-lays out
/// without an explicit per-pill x table.
fn draw_tab_pill(
    frame: &mut Frame,
    font: &Font,
    tab: ProfileTab,
    active: ProfileTab,
    strip_y: i32,
    idx: usize,
    visible_tabs: &[ProfileTab],
    state: &mut ButtonState,
    ctx: &UiCtx,
) -> Option<ProfileTab> {
    const PILL_H: i32 = 32;
    const PILL_PAD_Y: i32 = (TAB_STRIP_H - PILL_H) / 2;
    const SCALE: f32 = 0.95;
    const INNER_PAD_X: i32 = 12;
    const GAP: i32 = 6;
    const STRIP_LEFT_PAD: i32 = 8;

    let _ = tab; // tab is reachable as visible_tabs[idx]; kept in
    // signature for clarity at call site.

    // Pre-measure visible-tab widths so we can compute this pill's x.
    let mut x = STRIP_LEFT_PAD;
    for (i, t) in visible_tabs.iter().enumerate() {
        let (lw, _) = frame.measure_text(font, SCALE, t.label());
        let pw = lw + 2 * INNER_PAD_X;
        if i == idx {
            // Render this pill.
            let is_active = *t == active;
            let bg = if is_active { theme::ACCENT } else { theme::FIELD_BG };
            let fg = if is_active { theme::TEXT_PRIMARY } else { theme::TEXT_MUTED };
            let py = strip_y + PILL_PAD_Y;
            frame.fill_rect(x as f32, py as f32, pw as f32, PILL_H as f32, bg);
            // Text vertically centered in pill: baseline at py + (PILL_H + ascender)/2 ish.
            let (_, lh) = frame.measure_text(font, SCALE, t.label());
            let tx = x + INNER_PAD_X;
            let ty = py + (PILL_H + lh) / 2 - 4;
            frame.draw_text(font, tx, ty, fg, SCALE, t.label());
            // Hit-test.
            let rect = Rect::new(x as f32, py as f32, pw as f32, PILL_H as f32);
            let pressed_now = ctx.touches.iter().any(|tt| rect.contains(tt.x, tt.y));
            let clicked = state.pressed_last && !pressed_now && ctx.touches.is_empty();
            state.pressed_last = pressed_now;
            if clicked && !is_active {
                return Some(*t);
            }
            return None;
        }
        x += pw + GAP;
    }
    None
}

/// Mirrors `timeline.rs::placeholder_color`. Stable pastel color per
/// handle. Inlined here to avoid a public re-export from timeline.
/// Render the Posts tab's content rows below the (possibly-pinned)
/// pill strip. Walks `state.posts` with their lazy-measured heights;
/// rows that fall entirely above `pill_bottom` (scrolled past) or
/// below `viewport_bottom` (not yet visible) are skipped. The pinned
/// post (if any) gets a small "Pinned" badge above its row. Reuses
/// `crate::timeline::draw_post_row` for the actual row rendering.
fn draw_posts_content(
    frame: &mut Frame,
    font: &Font,
    state: &TabFeedState,
    ctx: &UiCtx,
    content_top_y: i32,
    pill_bottom: i32,
    viewport_bottom: i32,
) {
    use crate::timeline::draw_post_row;

    if let Some(err) = state.error.as_deref() {
        frame.draw_text_centered(
            font,
            pill_bottom + 30,
            theme::ERROR,
            0.95,
            "Could not load posts",
        );
        frame.draw_text_centered(
            font,
            pill_bottom + 60,
            theme::TEXT_MUTED,
            0.85,
            err,
        );
        return;
    }
    if state.posts.is_empty() {
        let label = if state.fetching { "Loading posts…" } else { "No posts yet." };
        frame.draw_text_centered(font, pill_bottom + 30, theme::TEXT_MUTED, 0.95, label);
        return;
    }

    let mut row_y = content_top_y;
    for (i, post) in state.posts.iter().enumerate() {
        let row_h = state.row_heights.get(i).copied().unwrap_or(0);
        if row_h == 0 {
            row_y += row_h;
            continue;
        }
        let row_bottom = row_y + row_h;
        // Skip if entirely above the pill strip (already scrolled
        // past) or below the viewport bottom.
        if row_bottom <= pill_bottom {
            row_y = row_bottom;
            continue;
        }
        if row_y >= viewport_bottom {
            break;
        }
        // Pinned-post badge above the row, if this is the pinned
        // entry. Render in ACCENT to match the verified-✓ tone.
        if state.pinned_idx == Some(i) {
            frame.draw_text(
                font,
                64,
                row_y + 2 + 14,
                theme::ACCENT,
                0.75,
                "📌 Pinned",
            );
        }
        draw_post_row(
            frame,
            font,
            post,
            row_y,
            row_h,
            ctx.emoji,
            ctx.texture_cache,
            ctx.avatar_mask,
            ctx.avatar_mask_field,
            i == state.selected_idx,
        );
        row_y = row_bottom;
    }
}

/// Render the Feeds tab content — fixed-height rows showing each
/// custom feed generator with avatar + display name + creator handle
/// + like-count chip. Reuses the same skip-rows-outside-viewport
/// pattern as `draw_posts_content`.
fn draw_feeds_content(
    frame: &mut Frame,
    font: &Font,
    state: &FeedsTabState,
    ctx: &UiCtx,
    content_top_y: i32,
    pill_bottom: i32,
    viewport_bottom: i32,
) {
    if let Some(err) = state.error.as_deref() {
        frame.draw_text_centered(
            font,
            pill_bottom + 30,
            theme::ERROR,
            0.95,
            "Could not load feeds",
        );
        frame.draw_text_centered(font, pill_bottom + 60, theme::TEXT_MUTED, 0.85, err);
        return;
    }
    if state.items.is_empty() {
        let label = if state.fetching { "Loading feeds…" } else { "No feeds yet." };
        frame.draw_text_centered(font, pill_bottom + 30, theme::TEXT_MUTED, 0.95, label);
        return;
    }
    // Dispatch avatar fetches for the visible rows so feed-creator
    // avatars get into the cache.
    if let Some(worker) = ctx.worker {
        for (i, item) in state.items.iter().enumerate() {
            let row_y = content_top_y + (i as i32) * NON_POST_ROW_H;
            let row_bottom = row_y + NON_POST_ROW_H;
            if row_bottom > pill_bottom && row_y < viewport_bottom {
                if let Some(url) = item.avatar.as_deref().map(avatar_thumbnail_jpeg) {
                    if !ctx.texture_cache.contains(&url) {
                        worker.send(WorkRequest::FetchImage { url });
                    }
                }
            }
        }
    }
    let mut row_y = content_top_y;
    for (i, item) in state.items.iter().enumerate() {
        let row_bottom = row_y + NON_POST_ROW_H;
        if row_bottom <= pill_bottom {
            row_y = row_bottom;
            continue;
        }
        if row_y >= viewport_bottom {
            break;
        }
        draw_feed_row(frame, font, item, row_y, ctx, i == state.selected_idx);
        row_y = row_bottom;
    }
}

/// Render one Feeds-tab row: 48px avatar + display name + creator
/// handle + like-count chip on the right. Selection is indicated
/// with a left-edge ACCENT bar + faint FIELD_BG row tint, mirroring
/// the post row's selection style.
fn draw_feed_row(
    frame: &mut Frame,
    font: &Font,
    item: &GeneratorView,
    y_top: i32,
    ctx: &UiCtx,
    is_selected: bool,
) {
    const PAD_X: i32 = 16;
    const AVATAR: i32 = NON_POST_AVATAR_SIZE;
    if is_selected {
        frame.fill_rect(
            0.0,
            y_top as f32,
            SCREEN_WIDTH as f32,
            NON_POST_ROW_H as f32,
            theme::FIELD_BG,
        );
        frame.fill_rect(
            0.0,
            y_top as f32,
            3.0,
            NON_POST_ROW_H as f32,
            theme::ACCENT,
        );
    }
    // Avatar — circular crop courtesy of the alpha-mask applied at
    // texture-cache insertion time.
    let avatar_x = PAD_X;
    let avatar_y = y_top + (NON_POST_ROW_H - AVATAR) / 2;
    let cx = (avatar_x + AVATAR / 2) as f32;
    let cy = (avatar_y + AVATAR / 2) as f32;
    let mut painted = false;
    if let Some(url) = item.avatar.as_deref().map(avatar_thumbnail_jpeg) {
        if let Some(tex) = ctx.texture_cache.get(&url) {
            let sx = AVATAR as f32 / tex.width().max(1) as f32;
            let sy = AVATAR as f32 / tex.height().max(1) as f32;
            frame.draw_texture_scale(tex, avatar_x as f32, avatar_y as f32, sx, sy);
            painted = true;
        }
    }
    if !painted {
        frame.fill_circle(
            cx,
            cy,
            (AVATAR / 2) as f32,
            placeholder_color(item.display_name.as_str()),
        );
    }
    // Name + creator handle (right of avatar).
    let text_x = avatar_x + AVATAR + 12;
    frame.draw_text(
        font,
        text_x,
        y_top + 28,
        theme::TEXT_PRIMARY,
        1.0,
        item.display_name.as_str(),
    );
    let creator_handle = format!("by @{}", item.creator.handle.as_str());
    frame.draw_text(
        font,
        text_x,
        y_top + 52,
        theme::TEXT_MUTED,
        0.85,
        &creator_handle,
    );
    // Like-count chip on the right.
    if let Some(likes) = item.like_count {
        let line = format!("♥ {}", likes);
        let (lw, _) = frame.measure_text(font, 0.85, &line);
        frame.draw_text(
            font,
            SCREEN_WIDTH - PAD_X - lw,
            y_top + 40,
            theme::TEXT_MUTED,
            0.85,
            &line,
        );
    }
    // Bottom separator.
    frame.fill_rect(
        PAD_X as f32,
        (y_top + NON_POST_ROW_H - 1) as f32,
        (SCREEN_WIDTH - 2 * PAD_X) as f32,
        1.0,
        theme::FIELD_BG,
    );
}

/// Render the Lists tab — fixed-height rows showing each list with
/// avatar + name + member count + creator handle.
fn draw_lists_content(
    frame: &mut Frame,
    font: &Font,
    state: &ListsTabState,
    ctx: &UiCtx,
    content_top_y: i32,
    pill_bottom: i32,
    viewport_bottom: i32,
) {
    if let Some(err) = state.error.as_deref() {
        frame.draw_text_centered(
            font,
            pill_bottom + 30,
            theme::ERROR,
            0.95,
            "Could not load lists",
        );
        frame.draw_text_centered(font, pill_bottom + 60, theme::TEXT_MUTED, 0.85, err);
        return;
    }
    if state.items.is_empty() {
        let label = if state.fetching { "Loading lists…" } else { "No lists yet." };
        frame.draw_text_centered(font, pill_bottom + 30, theme::TEXT_MUTED, 0.95, label);
        return;
    }
    if let Some(worker) = ctx.worker {
        for (i, item) in state.items.iter().enumerate() {
            let row_y = content_top_y + (i as i32) * NON_POST_ROW_H;
            let row_bottom = row_y + NON_POST_ROW_H;
            if row_bottom > pill_bottom && row_y < viewport_bottom {
                if let Some(url) = item.avatar.as_deref().map(avatar_thumbnail_jpeg) {
                    if !ctx.texture_cache.contains(&url) {
                        worker.send(WorkRequest::FetchImage { url });
                    }
                }
            }
        }
    }
    let mut row_y = content_top_y;
    for (i, item) in state.items.iter().enumerate() {
        let row_bottom = row_y + NON_POST_ROW_H;
        if row_bottom <= pill_bottom {
            row_y = row_bottom;
            continue;
        }
        if row_y >= viewport_bottom {
            break;
        }
        draw_list_row(frame, font, item, row_y, ctx, i == state.selected_idx);
        row_y = row_bottom;
    }
}

/// Render one Lists-tab row.
fn draw_list_row(
    frame: &mut Frame,
    font: &Font,
    item: &ListView,
    y_top: i32,
    ctx: &UiCtx,
    is_selected: bool,
) {
    const PAD_X: i32 = 16;
    const AVATAR: i32 = NON_POST_AVATAR_SIZE;
    if is_selected {
        frame.fill_rect(
            0.0,
            y_top as f32,
            SCREEN_WIDTH as f32,
            NON_POST_ROW_H as f32,
            theme::FIELD_BG,
        );
        frame.fill_rect(0.0, y_top as f32, 3.0, NON_POST_ROW_H as f32, theme::ACCENT);
    }
    let avatar_x = PAD_X;
    let avatar_y = y_top + (NON_POST_ROW_H - AVATAR) / 2;
    let cx = (avatar_x + AVATAR / 2) as f32;
    let cy = (avatar_y + AVATAR / 2) as f32;
    let mut painted = false;
    if let Some(url) = item.avatar.as_deref().map(avatar_thumbnail_jpeg) {
        if let Some(tex) = ctx.texture_cache.get(&url) {
            let sx = AVATAR as f32 / tex.width().max(1) as f32;
            let sy = AVATAR as f32 / tex.height().max(1) as f32;
            frame.draw_texture_scale(tex, avatar_x as f32, avatar_y as f32, sx, sy);
            painted = true;
        }
    }
    if !painted {
        frame.fill_circle(
            cx,
            cy,
            (AVATAR / 2) as f32,
            placeholder_color(item.name.as_str()),
        );
    }
    let text_x = avatar_x + AVATAR + 12;
    frame.draw_text(
        font,
        text_x,
        y_top + 28,
        theme::TEXT_PRIMARY,
        1.0,
        item.name.as_str(),
    );
    let creator_handle = format!("by @{}", item.creator.handle.as_str());
    frame.draw_text(
        font,
        text_x,
        y_top + 52,
        theme::TEXT_MUTED,
        0.85,
        &creator_handle,
    );
    if let Some(count) = item.list_item_count {
        let line = format!("{} members", count);
        let (lw, _) = frame.measure_text(font, 0.85, &line);
        frame.draw_text(
            font,
            SCREEN_WIDTH - PAD_X - lw,
            y_top + 40,
            theme::TEXT_MUTED,
            0.85,
            &line,
        );
    }
    frame.fill_rect(
        PAD_X as f32,
        (y_top + NON_POST_ROW_H - 1) as f32,
        (SCREEN_WIDTH - 2 * PAD_X) as f32,
        1.0,
        theme::FIELD_BG,
    );
}

/// Render the Starter Packs tab — fixed-height rows showing each
/// pack's creator + join counts.
fn draw_packs_content(
    frame: &mut Frame,
    font: &Font,
    state: &PacksTabState,
    ctx: &UiCtx,
    content_top_y: i32,
    pill_bottom: i32,
    viewport_bottom: i32,
) {
    if let Some(err) = state.error.as_deref() {
        frame.draw_text_centered(
            font,
            pill_bottom + 30,
            theme::ERROR,
            0.95,
            "Could not load starter packs",
        );
        frame.draw_text_centered(font, pill_bottom + 60, theme::TEXT_MUTED, 0.85, err);
        return;
    }
    if state.items.is_empty() {
        let label = if state.fetching {
            "Loading starter packs…"
        } else {
            "No starter packs yet."
        };
        frame.draw_text_centered(font, pill_bottom + 30, theme::TEXT_MUTED, 0.95, label);
        return;
    }
    if let Some(worker) = ctx.worker {
        for (i, item) in state.items.iter().enumerate() {
            let row_y = content_top_y + (i as i32) * NON_POST_ROW_H;
            let row_bottom = row_y + NON_POST_ROW_H;
            if row_bottom > pill_bottom && row_y < viewport_bottom {
                if let Some(url) = item.creator.avatar.as_deref().map(avatar_thumbnail_jpeg) {
                    if !ctx.texture_cache.contains(&url) {
                        worker.send(WorkRequest::FetchImage { url });
                    }
                }
            }
        }
    }
    let mut row_y = content_top_y;
    for (i, item) in state.items.iter().enumerate() {
        let row_bottom = row_y + NON_POST_ROW_H;
        if row_bottom <= pill_bottom {
            row_y = row_bottom;
            continue;
        }
        if row_y >= viewport_bottom {
            break;
        }
        draw_pack_row(frame, font, item, row_y, ctx, i == state.selected_idx);
        row_y = row_bottom;
    }
}

/// Render one Starter-Pack row. Atrium's `StarterPackViewBasic` only
/// gives us the creator + counts directly — the pack's own name +
/// description live in the unparsed `record` field; v1 just shows
/// "Pack by @creator" + member/join counts.
fn draw_pack_row(
    frame: &mut Frame,
    font: &Font,
    item: &StarterPackViewBasic,
    y_top: i32,
    ctx: &UiCtx,
    is_selected: bool,
) {
    const PAD_X: i32 = 16;
    const AVATAR: i32 = NON_POST_AVATAR_SIZE;
    if is_selected {
        frame.fill_rect(
            0.0,
            y_top as f32,
            SCREEN_WIDTH as f32,
            NON_POST_ROW_H as f32,
            theme::FIELD_BG,
        );
        frame.fill_rect(0.0, y_top as f32, 3.0, NON_POST_ROW_H as f32, theme::ACCENT);
    }
    let avatar_x = PAD_X;
    let avatar_y = y_top + (NON_POST_ROW_H - AVATAR) / 2;
    let cx = (avatar_x + AVATAR / 2) as f32;
    let cy = (avatar_y + AVATAR / 2) as f32;
    let mut painted = false;
    if let Some(url) = item.creator.avatar.as_deref().map(avatar_thumbnail_jpeg) {
        if let Some(tex) = ctx.texture_cache.get(&url) {
            let sx = AVATAR as f32 / tex.width().max(1) as f32;
            let sy = AVATAR as f32 / tex.height().max(1) as f32;
            frame.draw_texture_scale(tex, avatar_x as f32, avatar_y as f32, sx, sy);
            painted = true;
        }
    }
    if !painted {
        frame.fill_circle(
            cx,
            cy,
            (AVATAR / 2) as f32,
            placeholder_color(item.creator.handle.as_str()),
        );
    }
    let text_x = avatar_x + AVATAR + 12;
    let title = format!("Pack by @{}", item.creator.handle.as_str());
    frame.draw_text(font, text_x, y_top + 28, theme::TEXT_PRIMARY, 1.0, &title);
    let mut sub_segs: Vec<String> = Vec::new();
    if let Some(c) = item.list_item_count {
        sub_segs.push(format!("{} members", c));
    }
    if let Some(c) = item.joined_all_time_count {
        sub_segs.push(format!("{} joined", c));
    }
    let sub = sub_segs.join("  ·  ");
    frame.draw_text(font, text_x, y_top + 52, theme::TEXT_MUTED, 0.85, &sub);
    frame.fill_rect(
        PAD_X as f32,
        (y_top + NON_POST_ROW_H - 1) as f32,
        (SCREEN_WIDTH - 2 * PAD_X) as f32,
        1.0,
        theme::FIELD_BG,
    );
}

fn placeholder_color(handle: &str) -> Color {
    const PALETTE: [Color; 8] = [
        Color::rgb(0xF8, 0x9A, 0x9A),
        Color::rgb(0xF8, 0xC1, 0x9A),
        Color::rgb(0xF8, 0xE8, 0x9A),
        Color::rgb(0x9A, 0xF8, 0xA0),
        Color::rgb(0x9A, 0xE0, 0xF8),
        Color::rgb(0x9A, 0xA0, 0xF8),
        Color::rgb(0xC4, 0x9A, 0xF8),
        Color::rgb(0xF8, 0x9A, 0xE0),
    ];
    let mut h: u32 = 2166136261;
    for b in handle.bytes() {
        h = h.wrapping_mul(16777619) ^ b as u32;
    }
    PALETTE[(h as usize) % PALETTE.len()]
}

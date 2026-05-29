//! Timeline screen — scrollable list of posts from the user's home feed.
//!
//! Phase 3.2 worker pattern:
//! 1. First frame after construction: dispatch `WorkRequest::FetchTimeline
//!    { cursor: None }`. Render "Loading timeline…" until response arrives.
//! 2. `handle_worker_response` appends the batch's posts and stashes the
//!    next cursor for pagination.
//! 3. Each frame: lazy-measure row heights for any newly-arrived posts;
//!    render visible rows (skip ones that are entirely above/below the
//!    viewport).
//! 4. When `scroll_y` approaches the bottom and we have a non-None next
//!    cursor and no in-flight request, dispatch
//!    `FetchTimeline { cursor: Some(c) }`.
//!
//! Input:
//! - D-pad up/down → discrete scroll nudges (80 px).
//! - Left analog stick Y → continuous scroll velocity.
//! - CIRCLE → back to ProfileScreen.
//!
//! Out of scope (Phase 3.x or later):
//! - Avatars (3.5).
//! - Color emoji (3.4).
//! - Inter font (3.3) — body text rendered in PGF at scale 1.0.
//! - Repost / reply context ("X reposted", "Replying to @Y").
//! - Tap-a-post → thread view (Phase 4+).

use std::collections::HashSet;
use std::sync::Arc;

use atrium_api::app::bsky::feed::defs::{
    FeedViewPost, FeedViewPostReasonRefs, ReplyRefParentRefs,
};
use atrium_api::types::{TryFromUnknown, Union, Unknown};
use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_input::buttons;
use bsky_render::{
    theme, Color, EmojiAtlas, Font, Frame, Texture, TextureCache, SCREEN_HEIGHT, SCREEN_WIDTH,
};
use bsky_worker::{FeedSource, ReplyTarget, SavedFeedPin, WorkRequest, WorkResponse};

use crate::cdn::avatar_thumbnail_jpeg;
use crate::compose::ComposeScreen;
use crate::profile::ProfileScreen;
use crate::screen::{Screen, ScreenAction};
use crate::tabbar::{TabBar, TopLevel, TAB_BAR_HEIGHT};
use crate::thread::ThreadScreen;
use crate::widget::{ButtonState, Rect, UiCtx};

/// Sticky header height (px). Posts render below this; the header is
/// drawn last so it covers any content that's scrolled into its zone.
const HEADER_H: i32 = 40;

/// Pill row height — Phase 5.1 horizontal feed picker between the
/// sticky header and the post list.
const PILL_ROW_H: i32 = 50;

/// Viewport for the post list — between the pill row at the top and
/// the tab bar at the bottom.
const VIEWPORT_TOP: i32 = HEADER_H + PILL_ROW_H;
const VIEWPORT_BOTTOM: i32 = SCREEN_HEIGHT - TAB_BAR_HEIGHT;
const VIEWPORT_H: i32 = VIEWPORT_BOTTOM - VIEWPORT_TOP;

/// Side margins inside a post row.
pub(crate) const ROW_PAD_X: i32 = 16;
/// Avatar dimensions in the timeline post row.
pub(crate) const AVATAR_SIZE: i32 = 48;
/// Where text starts inside a row, accounting for the avatar slot:
/// ROW_PAD_X (left margin) + AVATAR_SIZE + 8 px gutter.
pub(crate) const TEXT_LEFT: i32 = ROW_PAD_X + AVATAR_SIZE + 8;
/// Vertical padding inside a row (above display name, below counts).
/// 20 px clears the ascender of 20 px text (ascender ~15–16 px above
/// baseline), preventing letter tops from overlapping the previous
/// row's separator. Was 12 in the PGF era.
pub(crate) const ROW_PAD_Y: i32 = 20;
/// Gap between display-name row and post body.
pub(crate) const TOP_LINE_H: i32 = 24;
/// Gap between post body and counts row.
pub(crate) const BODY_GAP: i32 = 8;
/// Counts row height + bottom padding + separator gap.
pub(crate) const FOOTER_H: i32 = 28;
/// Height of the optional repost/reply context line drawn above a post
/// row (one line at scale 0.85 + a little padding). Added on top of the
/// row when [`post_context_label`] returns `Some`.
pub(crate) const CONTEXT_LINE_H: i32 = 22;

/// Pixels per d-pad press.
const DPAD_STEP: f32 = 80.0;
/// Analog-stick deadzone (raw i8 magnitude). Vita sticks drift around
/// center by ±15–30 raw units depending on wear; 32 covers most.
const STICK_DEADZONE: i8 = 32;
/// Analog-stick scale: smaller = faster scroll. 24 ≈ ~5 px/frame at full
/// deflection above the deadzone (96 / 24 = 4 px/frame). Slightly
/// snappier than 32 to make up for the larger deadzone eating some of
/// the bottom of the curve.
const STICK_DIVISOR: f32 = 24.0;
/// Trigger pagination when within this many px of the end.
const PAGINATION_THRESHOLD: i32 = 600;

enum TimelineState {
    /// First batch in flight.
    Loading,
    /// At least one batch received. `next_cursor: None` ⇒ end of feed.
    Loaded {
        posts: Vec<FeedViewPost>,
        next_cursor: Option<String>,
    },
    /// Initial fetch failed. Subsequent-page failures don't enter this
    /// state (we just clear `fetching_more` and let the user re-trigger
    /// by scrolling).
    Error(String),
}

pub struct TimelineScreen {
    client: Arc<AuthClient>,
    state: TimelineState,
    /// Which feed is currently displayed. Initial value is
    /// `FeedSource::Following`; changed when the user taps a pill.
    /// Worker responses (`FeedPage { source, .. }`) are dropped if
    /// `source != current_source` (stale fetch).
    current_source: FeedSource,
    /// Pinned saved feeds, populated by the one-shot
    /// `FetchSavedFeeds`. Always begins with a `Following` pin
    /// (synthesized by the worker if absent from prefs). Empty until
    /// the response lands; the pill row renders nothing in that
    /// interval.
    saved_feeds: Vec<SavedFeedPin>,
    /// Hit-test state per pill, parallel to `saved_feeds`.
    pill_buttons: Vec<ButtonState>,
    /// Have we dispatched the one-shot `FetchSavedFeeds`?
    saved_feeds_dispatched: bool,
    /// Scroll offset in pixels from the top of the post list (0 = first
    /// post at y=VIEWPORT_TOP).
    scroll_y: f32,
    /// Have we sent the initial `FetchFeed` for `current_source`?
    /// Reset to `false` when the user switches feeds so the next frame
    /// re-dispatches.
    dispatched: bool,
    /// A pagination request is in flight; suppress further dispatches.
    fetching_more: bool,
    /// Cached row heights, parallel to `posts` in TimelineState::Loaded.
    /// Lazily extended in `frame()` when posts.len() > row_heights.len().
    row_heights: Vec<i32>,
    /// Avatar URLs we've dispatched fetches for AND haven't received a
    /// successful response for. On Ok response → cleared (allows re-fetch
    /// after eviction). On Err response → kept (don't retry-storm a
    /// failing URL during this session).
    inflight_avatars: HashSet<String>,
    /// Index into `posts` of the currently-focused row. D-pad up/down
    /// moves this; gamepad shortcuts (L = like, R = reply, TRIANGLE =
    /// repost) act on this row. The viewport auto-scrolls to keep the
    /// focused row visible. Defaults to 0 on screen entry.
    selected_idx: usize,
    tab_bar: TabBar,
    /// `true` when this instance was pushed via `with_feed` (e.g. the
    /// user tapped a Feeds-tab row on a profile). Pushed instances
    /// behave as sub-screens: no tab bar, CIRCLE pops back, no
    /// pill-row above the feed (since it's targeting a specific feed
    /// rather than offering feed switching).
    is_pushed: bool,
}

impl TimelineScreen {
    pub fn new(client: Arc<AuthClient>) -> Self {
        Self::with_source(client, FeedSource::Following, false)
    }

    /// Construct a `TimelineScreen` pre-loaded with a custom feed
    /// source (an `at://…/app.bsky.feed.generator/…` URI). Used when
    /// the ProfileScreen's Feeds tab is tapped and we want to push a
    /// timeline showing that feed's content. The pushed instance
    /// hides the bottom tab bar + pill row and treats CIRCLE as Pop.
    pub fn with_feed(client: Arc<AuthClient>, feed_uri: String) -> Self {
        Self::with_source(client, FeedSource::Feed(feed_uri), true)
    }

    fn with_source(client: Arc<AuthClient>, source: FeedSource, is_pushed: bool) -> Self {
        Self {
            client,
            state: TimelineState::Loading,
            current_source: source,
            saved_feeds: Vec::new(),
            pill_buttons: Vec::new(),
            saved_feeds_dispatched: false,
            scroll_y: 0.0,
            dispatched: false,
            fetching_more: false,
            row_heights: Vec::new(),
            inflight_avatars: HashSet::new(),
            selected_idx: 0,
            tab_bar: TabBar::new(TopLevel::Home),
            is_pushed,
        }
    }

    /// Switch to a new feed source. Resets paging state so the next
    /// frame re-dispatches the initial page.
    fn switch_to(&mut self, source: FeedSource) {
        if source == self.current_source {
            return;
        }
        self.current_source = source;
        self.state = TimelineState::Loading;
        self.dispatched = false;
        self.fetching_more = false;
        self.row_heights.clear();
        self.scroll_y = 0.0;
        self.selected_idx = 0;
    }

    /// Display name to render in the sticky header — the active pin's
    /// `display_name`, falling back to "Following" before saved feeds
    /// have arrived.
    fn header_label(&self) -> &str {
        self.saved_feeds
            .iter()
            .find(|p| p.source == self.current_source)
            .map(|p| p.display_name.as_str())
            .unwrap_or("Following")
    }

    /// Toggle the focused post's like state optimistically + dispatch
    /// CreateLike / DeleteLike. The local state updates IMMEDIATELY so
    /// the UI feels snappy; the worker confirms the new URI (for a
    /// create) or just acks (for a delete). On Err the optimistic
    /// change stays — user will see the real state on next refresh.
    fn toggle_like(&mut self, ctx: &UiCtx) {
        let Some(worker) = ctx.worker else { return };
        let TimelineState::Loaded { posts, .. } = &mut self.state else { return };
        let Some(post) = posts.get_mut(self.selected_idx) else { return };
        toggle_engagement(post, worker, EngagementKind::Like);
    }

    fn toggle_repost(&mut self, ctx: &UiCtx) {
        let Some(worker) = ctx.worker else { return };
        let TimelineState::Loaded { posts, .. } = &mut self.state else { return };
        let Some(post) = posts.get_mut(self.selected_idx) else { return };
        toggle_engagement(post, worker, EngagementKind::Repost);
    }

    /// Build a `ReplyTarget` for the currently-focused post. Phase 4.2
    /// MVP uses parent for both parent and root (acceptable for direct
    /// replies to top-level posts; thread replies render in the right
    /// place visually but the `root` field may be slightly wrong).
    /// Phase 4.4 (thread view) reads the parent's record.reply.root
    /// and threads it through here.
    fn focused_reply_target(&self) -> Option<ReplyTarget> {
        let posts = match &self.state {
            TimelineState::Loaded { posts, .. } => posts,
            _ => return None,
        };
        let post = posts.get(self.selected_idx)?;
        let uri = post.post.uri.clone();
        let cid = post.post.cid.as_ref().to_string();
        Some(ReplyTarget {
            parent_uri: uri.clone(),
            parent_cid: cid.clone(),
            root_uri: uri,
            root_cid: cid,
        })
    }

    /// True if the currently-selected post's row intersects the
    /// viewport at all (any pixel of it is on screen).
    fn is_selected_visible(&self) -> bool {
        if self.selected_idx >= self.row_heights.len() {
            return true;
        }
        let row_top: i32 = self.row_heights[..self.selected_idx].iter().sum();
        let row_h = self.row_heights[self.selected_idx];
        let view_top = self.scroll_y as i32;
        let view_bottom = view_top + VIEWPORT_H;
        row_top + row_h > view_top && row_top < view_bottom
    }
}

/// First post index whose row intersects the viewport given the
/// current `scroll_y`. Used to snap the selection when the user moves
/// the analog stick away from the selected post and then presses
/// d-pad.
fn first_visible_idx(row_heights: &[i32], scroll_y: i32) -> usize {
    let mut y = 0;
    for (i, &h) in row_heights.iter().enumerate() {
        if y + h > scroll_y {
            return i;
        }
        y += h;
    }
    row_heights.len().saturating_sub(1)
}

#[derive(Copy, Clone)]
pub(crate) enum EngagementKind {
    Like,
    Repost,
}

/// Pending tap result from the touch hit-test pass. Built up while
/// iterating posts (immutable borrow), then applied after the loop
/// finishes (mutable borrow of `self`).
/// Per-row tap-action result from `detect_post_tap_action`. The `idx`
/// fields refer to the index of the tapped post within whatever list
/// the caller is iterating (timeline state's `posts`, profile-tab's
/// `posts`, etc.).
pub(crate) enum TapAction {
    OpenProfile(String),
    OpenThread(String),
    ToggleLike(usize),
    ToggleRepost(usize),
    OpenVideo(crate::embeds::VideoTarget),
    OpenImage {
        images: Vec<crate::embeds::ViewerImage>,
        index: usize,
    },
}

/// Hit-test one post row at `(0..SCREEN_WIDTH) × (row_y..row_y+row_h)`
/// against the given touches, returning the appropriate tap action if
/// any zone is hit. Zone precedence: author region → counts (likes /
/// reposts) → video embed → quote embed → body. `idx` is echoed back
/// in `ToggleLike`/`ToggleRepost` actions so the caller knows which
/// post to mutate.
///
/// Reused across TimelineScreen, ThreadScreen, SearchScreen, and
/// ProfileScreen — every screen that renders post rows.
pub(crate) fn detect_post_tap_action(
    frame: &Frame,
    font: &Font,
    post: &FeedViewPost,
    row_y: i32,
    row_h: i32,
    touches: &[(i32, i32)],
    emoji: Option<&bsky_render::EmojiAtlas>,
    idx: usize,
    show_context: bool,
) -> Option<TapAction> {
    // Top-anchored zones (author, body, embeds) shift down by the
    // context-line height, matching draw_post_row. The counts row is
    // bottom-anchored (row_y + row_h - FOOTER_H), so it's unaffected.
    let ctx_h = context_line_height(post, show_context);
    let content_y = row_y + ctx_h;
    let author_rect = Rect::new(
        0.0,
        (content_y + ROW_PAD_Y) as f32,
        TEXT_LEFT as f32,
        AVATAR_SIZE as f32,
    );
    if touches.iter().any(|&(x, y)| author_rect.contains(x, y)) {
        return Some(TapAction::OpenProfile(
            post.post.author.handle.as_str().to_string(),
        ));
    }
    let counts_y = row_y + row_h - FOOTER_H + 4;
    let counts_h = 24.0;
    let likes_rect = Rect::new(0.0, counts_y as f32, 280.0, counts_h);
    let reposts_rect = Rect::new(280.0, counts_y as f32, 280.0, counts_h);
    if touches.iter().any(|&(x, y)| likes_rect.contains(x, y)) {
        return Some(TapAction::ToggleLike(idx));
    }
    if touches.iter().any(|&(x, y)| reposts_rect.contains(x, y)) {
        return Some(TapAction::ToggleRepost(idx));
    }
    if let Some(target) = crate::embeds::video_in_embed(
        post.post.embed.as_ref(),
        post.post.author.did.as_ref(),
    ) {
        if let Some((ey, eh)) = crate::embeds::embed_rect(frame, font, post, content_y, emoji) {
            let er = Rect::new(
                TEXT_LEFT as f32,
                ey as f32,
                (SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X) as f32,
                eh as f32,
            );
            if touches.iter().any(|&(x, y)| er.contains(x, y)) {
                return Some(TapAction::OpenVideo(target));
            }
        }
    }
    if let Some(quote_uri) = crate::embeds::quote_uri_in_embed(post.post.embed.as_ref()) {
        if let Some((ey, eh)) = crate::embeds::embed_rect(frame, font, post, content_y, emoji) {
            let embed_rect = Rect::new(
                TEXT_LEFT as f32,
                ey as f32,
                (SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X) as f32,
                eh as f32,
            );
            if touches.iter().any(|&(x, y)| embed_rect.contains(x, y)) {
                return Some(TapAction::OpenThread(quote_uri));
            }
        }
    }
    // Image embed → open the tapped image in the full-screen viewer.
    if let Some((images, rects)) = crate::embeds::image_tap_cells(frame, font, post, content_y, emoji) {
        for (i, &(rx, ry, rw, rh)) in rects.iter().enumerate() {
            if touches
                .iter()
                .any(|&(x, y)| x >= rx && x < rx + rw && y >= ry && y < ry + rh)
            {
                return Some(TapAction::OpenImage { images, index: i });
            }
        }
    }
    let body_rect = Rect::new(
        TEXT_LEFT as f32,
        (content_y + ROW_PAD_Y) as f32,
        (SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X) as f32,
        (row_h - ctx_h - ROW_PAD_Y - FOOTER_H) as f32,
    );
    if touches.iter().any(|&(x, y)| body_rect.contains(x, y)) {
        return Some(TapAction::OpenThread(post.post.uri.clone()));
    }
    None
}

/// Sentinel URI used while a CreateLike / CreateRepost is in flight.
/// The real AT-URI replaces it on `LikeChanged(Ok(Some(uri)))` /
/// `RepostChanged(Ok(Some(uri)))` so subsequent un-like / un-repost
/// can extract the rkey.
pub(crate) const PENDING_URI: &str = "__pending__";

/// Apply optimistic toggle + dispatch the matching Create/Delete worker
/// request. If `kind == Like`, mutates `viewer.like` + `like_count`;
/// `Repost` mutates `viewer.repost` + `repost_count`.
pub(crate) fn toggle_engagement(
    post: &mut FeedViewPost,
    worker: &bsky_worker::Worker,
    kind: EngagementKind,
) {
    use atrium_api::app::bsky::feed::defs::ViewerStateData;

    let post_uri = post.post.uri.clone();
    let post_cid = post.post.cid.as_ref().to_string();
    let view = &mut post.post;
    if view.viewer.is_none() {
        view.viewer = Some(
            ViewerStateData {
                bookmarked: None,
                embedding_disabled: None,
                like: None,
                pinned: None,
                reply_disabled: None,
                repost: None,
                thread_muted: None,
            }
            .into(),
        );
    }
    // Take the existing engagement URI (if any) and stamp the new
    // optimistic state in the viewer. Done before mutating
    // like_count/repost_count so the borrow on viewer ends first.
    let existing = {
        let viewer = view.viewer.as_mut().expect("viewer just initialized");
        let slot = match kind {
            EngagementKind::Like => &mut viewer.like,
            EngagementKind::Repost => &mut viewer.repost,
        };
        let prev = slot.take();
        if prev.is_none() {
            *slot = Some(PENDING_URI.to_string());
        }
        prev
    };
    let count = match kind {
        EngagementKind::Like => &mut view.like_count,
        EngagementKind::Repost => &mut view.repost_count,
    };
    if let Some(existing_uri) = existing {
        // Was engaged; now disengaged.
        *count = Some((count.unwrap_or(0) - 1).max(0));
        if existing_uri == PENDING_URI {
            // Create still in flight — drop the un-engage. Server may
            // end up with a stray record (Phase 4.x can queue a delete
            // for after the create response arrives).
            return;
        }
        let Some(rkey) = existing_uri.rsplit('/').next().map(String::from) else { return };
        match kind {
            EngagementKind::Like => worker.send(WorkRequest::DeleteLike { rkey }),
            EngagementKind::Repost => worker.send(WorkRequest::DeleteRepost { rkey }),
        }
    } else {
        // Was idle; now engaging.
        *count = Some(count.unwrap_or(0) + 1);
        match kind {
            EngagementKind::Like => worker.send(WorkRequest::CreateLike {
                post_uri,
                post_cid,
            }),
            EngagementKind::Repost => worker.send(WorkRequest::CreateRepost {
                post_uri,
                post_cid,
            }),
        }
    }
}

impl Screen for TimelineScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        _ime: &mut Ime,
    ) -> ScreenAction {
        // ─── 1. Dispatch initial fetch on first frame. ─────────────────
        if !self.dispatched {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::FetchFeed {
                    source: self.current_source.clone(),
                    cursor: None,
                });
                self.dispatched = true;
            }
        }
        // Pushed instances target a specific feed and skip the saved-
        // feeds pill row, so no need to fetch the user's saved feeds.
        if !self.saved_feeds_dispatched && !self.is_pushed {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::FetchSavedFeeds);
                self.saved_feeds_dispatched = true;
            }
        }

        // ─── 2. Input: selection + scroll. ─────────────────────────────
        // CIRCLE on top-level is a no-op (tab bar handles top-level
        // navigation; only pushed sub-screens consume CIRCLE for back).
        // D-pad up/down moves the focused-post selection; auto-scroll
        // happens in step 4. Analog stick still does pixel-level
        // scrolling independently of selection.
        let post_count = match &self.state {
            TimelineState::Loaded { posts, .. } => posts.len(),
            _ => 0,
        };
        let mut selection_changed = false;
        if post_count > 0
            && (ctx.pad.just_pressed(buttons::UP) || ctx.pad.just_pressed(buttons::DOWN))
        {
            // If the current selection has scrolled off-screen (analog
            // stick moved the viewport away), snap selection to the
            // first visible post BEFORE applying the d-pad direction.
            // Without this, d-pad press auto-scrolls back to the
            // off-screen selection — visible jump.
            if !self.is_selected_visible() {
                self.selected_idx = first_visible_idx(&self.row_heights, self.scroll_y as i32);
                selection_changed = true;
            }
            if ctx.pad.just_pressed(buttons::UP) && self.selected_idx > 0 {
                self.selected_idx -= 1;
                selection_changed = true;
            }
            if ctx.pad.just_pressed(buttons::DOWN) && self.selected_idx + 1 < post_count {
                self.selected_idx += 1;
                selection_changed = true;
            }
        }
        // L = like (4.3), R = reply (4.2 — Push ComposeScreen with
        // reply context), TRIANGLE = repost (4.3), SQUARE = compose
        // a top-level post.
        if ctx.pad.just_pressed(buttons::L1) {
            self.toggle_like(ctx);
        }
        if ctx.pad.just_pressed(buttons::R1) {
            if let Some(reply) = self.focused_reply_target() {
                let handle = match &self.state {
                    TimelineState::Loaded { posts, .. } => posts
                        .get(self.selected_idx)
                        .map(|p| p.post.author.handle.as_str().to_string()),
                    _ => None,
                };
                return ScreenAction::Push(Box::new(ComposeScreen::new(
                    Arc::clone(&self.client),
                    Some(reply),
                    handle,
                )));
            }
        }
        if ctx.pad.just_pressed(buttons::TRIANGLE) {
            self.toggle_repost(ctx);
        }
        if ctx.pad.just_pressed(buttons::SQUARE) {
            return ScreenAction::Push(Box::new(ComposeScreen::new(
                Arc::clone(&self.client),
                None,
                None,
            )));
        }
        // Analog-stick scroll. Use a deadzone-subtract curve so motion
        // just past the deadzone produces tiny movement (no binary
        // jump), and motion at full deflection is fast. Without the
        // subtract, idle drift just inside the deadzone produces
        // perceptible scroll over time.
        let stick_y = ctx.pad.left_stick.1;
        let mag = stick_y.unsigned_abs() as f32;
        let dz = STICK_DEADZONE as f32;
        if mag > dz {
            let sign: f32 = if stick_y < 0 { -1.0 } else { 1.0 };
            let effective = (mag - dz) * sign;
            self.scroll_y += effective / STICK_DIVISOR;
        }
        // Suppress unused-const warnings for DPAD_STEP — it's now
        // implicit in the auto-scroll math below.
        let _ = DPAD_STEP;

        // ─── 3. Lazy-measure row heights for any newly-arrived posts. ─
        if let TimelineState::Loaded { posts, .. } = &self.state {
            while self.row_heights.len() < posts.len() {
                let i = self.row_heights.len();
                let h = measure_post_row(frame, font, &posts[i], ctx.emoji, true);
                self.row_heights.push(h);
            }
        }

        // ─── 4. Compute layout: total content height + scroll clamp.
        //        Auto-scroll only when d-pad selection just changed,
        //        so analog-stick scroll isn't fighting the selection
        //        snap every frame. ────────────────────────────────────
        let total_h: i32 = self.row_heights.iter().sum();
        let max_scroll = (total_h - VIEWPORT_H).max(0) as f32;
        if selection_changed && self.selected_idx < self.row_heights.len() {
            // Top-y of the selected row in content coords (before
            // subtracting scroll_y).
            let row_top: i32 = self.row_heights[..self.selected_idx].iter().sum();
            let row_h = self.row_heights[self.selected_idx];
            let view_top = self.scroll_y as i32;
            let view_bottom = view_top + VIEWPORT_H;
            const SCROLL_MARGIN: i32 = 50;
            if row_h > VIEWPORT_H {
                // Post is taller than the viewport — pin its top so the
                // user reads top-down. Snapping the bottom would put the
                // header off-screen above and feel disorienting.
                self.scroll_y = (row_top - SCROLL_MARGIN).max(0) as f32;
            } else if row_top < view_top + SCROLL_MARGIN {
                self.scroll_y = (row_top - SCROLL_MARGIN).max(0) as f32;
            } else if row_top + row_h > view_bottom - SCROLL_MARGIN {
                self.scroll_y =
                    (row_top + row_h + SCROLL_MARGIN - VIEWPORT_H).max(0) as f32;
            }
        }
        if self.scroll_y < 0.0 {
            self.scroll_y = 0.0;
        }
        if self.scroll_y > max_scroll {
            self.scroll_y = max_scroll;
        }

        // ─── 5. Pagination trigger. ───────────────────────────────────
        if !self.fetching_more {
            if let TimelineState::Loaded {
                next_cursor: Some(cursor),
                ..
            } = &self.state
            {
                let near_bottom =
                    self.scroll_y as i32 + VIEWPORT_H + PAGINATION_THRESHOLD >= total_h;
                if near_bottom {
                    if let Some(worker) = ctx.worker {
                        worker.send(WorkRequest::FetchFeed {
                            source: self.current_source.clone(),
                            cursor: Some(cursor.clone()),
                        });
                        self.fetching_more = true;
                    }
                }
            }
        }

        // ─── 5b. Avatar dispatch for visible posts. ───────────────────
        // For each post that will be drawn this frame, check if its
        // avatar URL is already cached or in flight. If neither, fire a
        // FetchImage request. The actual render uses the cache; misses
        // show a placeholder.
        if let TimelineState::Loaded { posts, .. } = &self.state {
            if let Some(worker) = ctx.worker {
                let mut y_probe = VIEWPORT_TOP - self.scroll_y as i32;
                for (post, &row_h) in posts.iter().zip(self.row_heights.iter()) {
                    let row_bottom = y_probe + row_h;
                    if row_bottom > VIEWPORT_TOP && y_probe < SCREEN_HEIGHT {
                        if let Some(url) = post.post.author.avatar.as_ref() {
                            // Transform the URL so cache lookup and fetch
                            // use the same vita2d-compatible JPEG variant.
                            let url = avatar_thumbnail_jpeg(url);
                            if !ctx.texture_cache.contains(&url)
                                && !self.inflight_avatars.contains(&url)
                            {
                                worker.send(WorkRequest::FetchImage { url: url.clone() });
                                self.inflight_avatars.insert(url);
                            }
                        }
                        // Embed image URLs (post images, link card thumb,
                        // video thumb, quote-author avatar). These are
                        // already CDN-resolved JPEGs.
                        for url in
                            crate::embeds::embed_image_urls(post.post.embed.as_ref())
                        {
                            if !ctx.texture_cache.contains(&url)
                                && !self.inflight_avatars.contains(&url)
                            {
                                worker.send(WorkRequest::FetchImage { url: url.clone() });
                                self.inflight_avatars.insert(url);
                            }
                        }
                    }
                    y_probe += row_h;
                }
            }
        }

        // ─── 5c. Avatar dispatch for pill row. ────────────────────────
        if let Some(worker) = ctx.worker {
            for pin in self.saved_feeds.iter() {
                if let Some(url) = pin.avatar_url.as_ref() {
                    let url = avatar_thumbnail_jpeg(url);
                    if !ctx.texture_cache.contains(&url)
                        && !self.inflight_avatars.contains(&url)
                    {
                        worker.send(WorkRequest::FetchImage { url: url.clone() });
                        self.inflight_avatars.insert(url);
                    }
                }
            }
        }

        // ─── 6. Render content (post list, then sticky header on top). ─
        match &self.state {
            TimelineState::Loading => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2,
                    theme::TEXT_MUTED,
                    1.1,
                    "Loading timeline…",
                );
            }
            TimelineState::Loaded { posts, next_cursor } => {
                if posts.is_empty() {
                    frame.draw_text_centered(
                        font,
                        SCREEN_HEIGHT / 2,
                        theme::TEXT_MUTED,
                        1.1,
                        "Your timeline is empty",
                    );
                } else {
                    draw_post_list(
                        frame,
                        font,
                        posts,
                        &self.row_heights,
                        self.scroll_y,
                        ctx.emoji,
                        ctx.texture_cache,
                        ctx.avatar_mask,
                        ctx.avatar_mask_field,
                        self.selected_idx,
                    );
                    if self.fetching_more {
                        let bottom_y = VIEWPORT_TOP + total_h - self.scroll_y as i32 + 8;
                        if bottom_y > VIEWPORT_TOP && bottom_y < SCREEN_HEIGHT - 8 {
                            frame.draw_text_centered(
                                font,
                                bottom_y,
                                theme::TEXT_MUTED,
                                0.9,
                                "Loading more…",
                            );
                        }
                    } else if next_cursor.is_none() {
                        // Reached end of feed: subtle marker at the bottom.
                        let bottom_y = VIEWPORT_TOP + total_h - self.scroll_y as i32 + 8;
                        if bottom_y > VIEWPORT_TOP && bottom_y < SCREEN_HEIGHT - 8 {
                            frame.draw_text_centered(
                                font,
                                bottom_y,
                                theme::TEXT_MUTED,
                                0.85,
                                "— end of timeline —",
                            );
                        }
                    }
                }
            }
            TimelineState::Error(msg) => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 - 20,
                    theme::ERROR,
                    1.0,
                    "Could not load timeline",
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

        // ─── 7. Pill row (drawn after posts so it covers any row that
        //        scrolled up into its zone). Hit-tested below. Pushed
        //        instances (e.g. opened from ProfileScreen's Feeds tab)
        //        skip the pill row since they target a single feed. ───
        let pill_tap_idx = if self.is_pushed {
            None
        } else {
            draw_pill_row(
                frame,
                font,
                &self.saved_feeds,
                &self.current_source,
                &mut self.pill_buttons,
                ctx,
            )
        };

        // ─── 8. Sticky header (drawn after pill row so it covers any
        //        pill content that exceeds its bounds). ──────────────────
        let header_label = self.header_label().to_string();
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, HEADER_H as f32, theme::FIELD_BG);
        frame.draw_text_centered(font, 26, theme::TEXT_PRIMARY, 1.1, &header_label);
        frame.fill_rect(
            0.0,
            HEADER_H as f32 - 1.0,
            SCREEN_WIDTH as f32,
            1.0,
            theme::TEXT_MUTED,
        );

        // Apply pill tap (after rendering, so this frame still shows
        // the previous feed; switch takes effect next frame).
        if let Some(idx) = pill_tap_idx {
            if let Some(pin) = self.saved_feeds.get(idx) {
                let new_source = pin.source.clone();
                self.switch_to(new_source);
            }
        }

        // ─── 8. Tab bar (last — covers any row that scrolled into its
        //        zone). Tap → switch top-level. Pushed instances skip
        //        the tab bar; CIRCLE pops back to whatever pushed us.
        if self.is_pushed {
            if ctx.pad.just_pressed(buttons::CIRCLE) {
                return ScreenAction::Pop;
            }
        } else if let Some(target) = self.tab_bar.render(frame, font, ctx) {
            return ScreenAction::SwitchTab(target);
        }

        // ─── 9. Tap detection on visible posts. Three target zones:
        //          - Author region (avatar + name) → Push ProfileScreen
        //          - Likes count area → toggle_like
        //          - Reposts count area → toggle_repost
        //        Hit-tested per-frame against current scroll position. ─
        if !ctx.touches.is_empty() {
            // Take snapshot of touches up-front so we can mutate state
            // while iterating posts immutably. Exclude the bottom tab-bar
            // band: the bar is drawn on top and handles those touches
            // itself, so a content tap must not fall through to a post row
            // that happens to extend under the bar.
            let touches: Vec<_> = ctx
                .touches
                .iter()
                .filter(|t| t.y < VIEWPORT_BOTTOM)
                .map(|t| (t.x, t.y))
                .collect();
            let mut tap_action: Option<TapAction> = None;
            if let TimelineState::Loaded { posts, .. } = &self.state {
                let mut y_probe = VIEWPORT_TOP - self.scroll_y as i32;
                for (idx, (post, &row_h)) in
                    posts.iter().zip(self.row_heights.iter()).enumerate()
                {
                    let row_bottom = y_probe + row_h;
                    if row_bottom > VIEWPORT_TOP && y_probe < VIEWPORT_BOTTOM {
                        // Feed rows always show repost/reply context, so
                        // shift top-anchored tap zones down by ctx_h to
                        // match draw_post_row. Counts stay bottom-anchored.
                        let ctx_h = context_line_height(post, true);
                        let content_y = y_probe + ctx_h;
                        let author_rect = Rect::new(
                            0.0,
                            (content_y + ROW_PAD_Y) as f32,
                            TEXT_LEFT as f32,
                            AVATAR_SIZE as f32,
                        );
                        if touches.iter().any(|&(x, y)| author_rect.contains(x, y)) {
                            tap_action = Some(TapAction::OpenProfile(
                                post.post.author.handle.as_str().to_string(),
                            ));
                            break;
                        }
                        // Counts row tap zones. counts_y is body_y +
                        // body_h + BODY_GAP — but body_h is computed
                        // inside draw_post_row from wrapped text. We
                        // approximate using row_h - FOOTER_H since the
                        // counts row sits in the footer block.
                        let counts_y = y_probe + row_h - FOOTER_H + 4;
                        let counts_h = 24.0;
                        // Roughly partition the bottom row into three:
                        // likes 0..280, reposts 280..560, replies 560..840.
                        let likes_rect =
                            Rect::new(0.0, counts_y as f32, 280.0, counts_h);
                        let reposts_rect =
                            Rect::new(280.0, counts_y as f32, 280.0, counts_h);
                        if touches.iter().any(|&(x, y)| likes_rect.contains(x, y)) {
                            tap_action = Some(TapAction::ToggleLike(idx));
                            break;
                        }
                        if touches.iter().any(|&(x, y)| reposts_rect.contains(x, y))
                        {
                            tap_action = Some(TapAction::ToggleRepost(idx));
                            break;
                        }
                        // Video-embed region (if any) → OpenVideo. Takes
                        // precedence over both quote and body taps so a
                        // tap on the ▶ placeholder opens the player.
                        if let Some(target) = crate::embeds::video_in_embed(
                            post.post.embed.as_ref(),
                            post.post.author.did.as_ref(),
                        ) {
                            if let Some((ey, eh)) =
                                crate::embeds::embed_rect(frame, font, post, content_y, ctx.emoji)
                            {
                                let er = Rect::new(
                                    TEXT_LEFT as f32,
                                    ey as f32,
                                    (SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X) as f32,
                                    eh as f32,
                                );
                                if touches.iter().any(|&(x, y)| er.contains(x, y)) {
                                    tap_action = Some(TapAction::OpenVideo(target));
                                    break;
                                }
                            }
                        }
                        // Quote-embed region (if any) → OpenThread of the
                        // quoted post. Checked BEFORE the body fallback so
                        // a tap on the quote card opens the right thread.
                        if let Some(quote_uri) =
                            crate::embeds::quote_uri_in_embed(post.post.embed.as_ref())
                        {
                            if let Some((ey, eh)) =
                                crate::embeds::embed_rect(frame, font, post, content_y, ctx.emoji)
                            {
                                let embed_rect = Rect::new(
                                    TEXT_LEFT as f32,
                                    ey as f32,
                                    (SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X) as f32,
                                    eh as f32,
                                );
                                if touches.iter().any(|&(x, y)| embed_rect.contains(x, y))
                                {
                                    tap_action = Some(TapAction::OpenThread(quote_uri));
                                    break;
                                }
                            }
                        }
                        // Image embed → full-screen viewer (before the body
                        // fallback so a tap on an image opens it, not the
                        // thread).
                        if let Some((images, rects)) =
                            crate::embeds::image_tap_cells(frame, font, post, content_y, ctx.emoji)
                        {
                            if let Some(i) = rects.iter().position(|&(rx, ry, rw, rh)| {
                                touches
                                    .iter()
                                    .any(|&(x, y)| x >= rx && x < rx + rw && y >= ry && y < ry + rh)
                            }) {
                                tap_action = Some(TapAction::OpenImage { images, index: i });
                                break;
                            }
                        }
                        // Body region: anywhere in the row that isn't
                        // the author region, counts row, or a quote
                        // embed → open thread for *this* post.
                        let body_rect = Rect::new(
                            TEXT_LEFT as f32,
                            (content_y + ROW_PAD_Y) as f32,
                            (SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X) as f32,
                            (row_h - ctx_h - ROW_PAD_Y - FOOTER_H) as f32,
                        );
                        if touches.iter().any(|&(x, y)| body_rect.contains(x, y)) {
                            tap_action = Some(TapAction::OpenThread(post.post.uri.clone()));
                            break;
                        }
                    }
                    y_probe += row_h;
                }
            }
            match tap_action {
                Some(TapAction::OpenProfile(handle)) => {
                    return ScreenAction::Push(Box::new(ProfileScreen::new(
                        Arc::clone(&self.client),
                        Some(handle),
                    )));
                }
                Some(TapAction::OpenThread(uri)) => {
                    return ScreenAction::Push(Box::new(ThreadScreen::new(
                        Arc::clone(&self.client),
                        uri,
                    )));
                }
                Some(TapAction::ToggleLike(idx)) => {
                    let prev_sel = self.selected_idx;
                    self.selected_idx = idx;
                    self.toggle_like(ctx);
                    self.selected_idx = prev_sel;
                }
                Some(TapAction::ToggleRepost(idx)) => {
                    let prev_sel = self.selected_idx;
                    self.selected_idx = idx;
                    self.toggle_repost(ctx);
                    self.selected_idx = prev_sel;
                }
                Some(TapAction::OpenVideo(target)) => {
                    return ScreenAction::Push(Box::new(crate::video_player::VideoPlayerScreen::new(
                        Arc::clone(&self.client),
                        target.did,
                        target.cid,
                    )));
                }
                Some(TapAction::OpenImage { images, index }) => {
                    return ScreenAction::Push(Box::new(
                        crate::image_viewer::ImageViewerScreen::new(images, index),
                    ));
                }
                None => {}
            }
        }

        ScreenAction::None
    }

    fn top_level(&self) -> Option<TopLevel> {
        if self.is_pushed { None } else { Some(TopLevel::Home) }
    }

    fn control_hints(&self) -> Vec<(&'static str, &'static str)> {
        let mut v = vec![
            ("SQUARE", "Compose"),
            ("L1", "Like"),
            ("TRIANGLE", "Repost"),
            ("R1", "Reply"),
        ];
        if self.is_pushed {
            v.push(("CIRCLE", "Back"));
        }
        v
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        match resp {
            // Feed page for the *currently shown* feed.
            WorkResponse::FeedPage { source, batch } if source == self.current_source => {
                match batch {
                    Ok(batch) => {
                        self.fetching_more = false;
                        match &mut self.state {
                            TimelineState::Loaded { posts, next_cursor } => {
                                posts.extend(batch.posts);
                                *next_cursor = batch.cursor;
                            }
                            _ => {
                                self.state = TimelineState::Loaded {
                                    posts: batch.posts,
                                    next_cursor: batch.cursor,
                                };
                            }
                        }
                    }
                    Err(e) => {
                        self.fetching_more = false;
                        if matches!(self.state, TimelineState::Loading) {
                            self.state = TimelineState::Error(e);
                        }
                        // Page-load failures past the first page: silent.
                        // User can scroll to retrigger.
                    }
                }
            }
            // Stale feed page (user switched feeds while in flight). Drop.
            WorkResponse::FeedPage { .. } => {}
            // Saved feeds populate the pill row.
            WorkResponse::SavedFeeds(Ok(b)) => {
                self.pill_buttons.clear();
                self.pill_buttons.resize_with(b.pins.len(), ButtonState::default);
                self.saved_feeds = b.pins;
            }
            WorkResponse::SavedFeeds(Err(e)) => {
                eprintln!("FetchSavedFeeds failed: {e} — falling back to Following-only");
                // saved_feeds stays empty → pill row renders nothing extra.
            }
            // Profile responses can arrive here if the previous screen
            // dispatched one and the user transitioned before the response
            // landed. Ignore them — ProfileScreen is gone.
            WorkResponse::Profile(_) => {}
            // Image responses. On Ok: cache populated by main.rs; clear
            // inflight so future evict + re-fetch works. On Err: KEEP
            // url in inflight_avatars permanently for this session, so
            // we don't retry-storm. (Phase 4 may add a manual retry.)
            WorkResponse::Image { url, bytes } => match bytes {
                Ok(_) => {
                    self.inflight_avatars.remove(&url);
                }
                Err(_) => {
                    // Leave url in inflight_avatars (don't retry).
                }
            },
            // PostCreated belongs to ComposeScreen — if it lands here
            // it's because the user popped out of compose before the
            // response arrived. Drop silently.
            WorkResponse::PostCreated(_) => {}
            // Like/Repost responses confirm an in-flight optimistic
            // toggle. Phase 4.3 doesn't track which post the response
            // belongs to (no Vec lookup); on Ok(Some(uri)) we walk the
            // posts and replace any PENDING_URI sentinel with the real
            // URI in the matching field. On Err / Ok(None) nothing to do
            // (delete-acks are no-ops; create-errs leave optimistic UI
            // showing as engaged — user sees real state on refresh).
            WorkResponse::LikeChanged(Ok(Some(uri))) => {
                if let TimelineState::Loaded { posts, .. } = &mut self.state {
                    for post in posts.iter_mut() {
                        if let Some(viewer) = post.post.viewer.as_mut() {
                            if viewer.like.as_deref() == Some(PENDING_URI) {
                                viewer.like = Some(uri);
                                break;
                            }
                        }
                    }
                }
            }
            WorkResponse::RepostChanged(Ok(Some(uri))) => {
                if let TimelineState::Loaded { posts, .. } = &mut self.state {
                    for post in posts.iter_mut() {
                        if let Some(viewer) = post.post.viewer.as_mut() {
                            if viewer.repost.as_deref() == Some(PENDING_URI) {
                                viewer.repost = Some(uri);
                                break;
                            }
                        }
                    }
                }
            }
            WorkResponse::LikeChanged(_) | WorkResponse::RepostChanged(_) => {
                // Delete acks (Ok(None)) and errors land here; no-op.
            }
            // Thread responses belong to ThreadScreen.
            WorkResponse::Thread(_) => {}
            // Follow responses belong to ProfileScreen.
            WorkResponse::FollowChanged(_) => {}
            // Notifications belong to NotificationsScreen.
            WorkResponse::Notifications(_) => {}
            // Search results belong to SearchScreen.
            WorkResponse::SearchActors(_) | WorkResponse::SearchPosts(_) => {}
            // Video blob responses belong to VideoPlayerScreen.
            WorkResponse::VideoBlob { .. } | WorkResponse::VideoBlobProgress { .. } => {}
            // Profile-tab content belongs to ProfileScreen.
            WorkResponse::ActorFeeds { .. }
            | WorkResponse::ActorLists { .. }
            | WorkResponse::ActorStarterPacks { .. } => {}
            // DM responses belong to the conversation screens.
            WorkResponse::Convos(_)
            | WorkResponse::ConvoMessages { .. }
            | WorkResponse::MessageSent { .. }
            | WorkResponse::ConvoForMembers(_)
            | WorkResponse::ConvoRead(_) => {}
        }
    }
}

/// Display name for a `ProfileViewBasic`, falling back to `@handle` when
/// the display name is absent or empty (mirrors the author idiom in
/// [`draw_post_row`]).
fn display_or_handle(profile: &atrium_api::app::bsky::actor::defs::ProfileViewBasic) -> String {
    profile
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("@{}", profile.handle.as_str()))
}

/// The feed-context label to draw above a post row, or `None` if the row
/// needs no context line. Drives both the row height and the draw.
///
/// - Reposts win: a reposted post shows "Reposted by X" even if it is
///   also a reply (the repost is why it's in the feed).
/// - Replies show "Reply to @handle", except self-thread continuations
///   (replying to yourself), which Bluesky surfaces as your own thread,
///   not a "reply to" — suppressed here.
/// - Deleted/blocked parents (`NotFoundPost`/`BlockedPost`), pinned posts
///   (`ReasonPin`, rendered separately by ProfileScreen), and ordinary
///   top-level posts yield `None`.
///
/// `show_context` is `false` for Thread (redundant) and Search (no
/// reason/reply data) callers, short-circuiting to `None`.
fn post_context_label(post: &FeedViewPost, show_context: bool) -> Option<String> {
    if !show_context {
        return None;
    }
    // Repost reason takes precedence.
    if let Some(Union::Refs(FeedViewPostReasonRefs::ReasonRepost(r))) = post.reason.as_ref() {
        return Some(format!("Reposted by {}", display_or_handle(&r.by)));
    }
    // Otherwise, a reply to someone other than the author.
    if let Some(reply) = post.reply.as_ref() {
        if let Union::Refs(ReplyRefParentRefs::PostView(parent)) = &reply.parent {
            if parent.author.did != post.post.author.did {
                return Some(format!("Reply to @{}", parent.author.handle.as_str()));
            }
        }
    }
    None
}

/// Height the context line adds to a row (0 when there's no label).
pub(crate) fn context_line_height(post: &FeedViewPost, show_context: bool) -> i32 {
    if post_context_label(post, show_context).is_some() {
        CONTEXT_LINE_H
    } else {
        0
    }
}

/// Compute one post row's total height (without drawing). Mirrors the
/// layout in [`draw_post_row`].
pub(crate) fn measure_post_row(
    frame: &Frame,
    font: &Font,
    post: &FeedViewPost,
    emoji: Option<&EmojiAtlas>,
    show_context: bool,
) -> i32 {
    // Body text wraps to the column right of the avatar slot.
    let inner_w = SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X;
    let body_text = extract_post_text(&post.post.record).unwrap_or_default();
    let body_h = frame.measure_text_wrapped_with_emoji(font, inner_w, 1.0, &body_text, emoji);
    let embed_h = crate::embeds::measure_embed_block(frame, font, post.post.embed.as_ref(), emoji);
    let bottom_gap = if embed_h > 0 {
        crate::embeds::EMBED_BOTTOM_GAP
    } else {
        BODY_GAP
    };
    let text_block_h =
        ROW_PAD_Y + TOP_LINE_H + body_h + embed_h + bottom_gap + FOOTER_H;
    // Ensure the row is at least as tall as the avatar slot.
    let avatar_block_h = ROW_PAD_Y + AVATAR_SIZE + FOOTER_H;
    // The repost/reply context line (if any) sits above everything.
    text_block_h.max(avatar_block_h) + context_line_height(post, show_context)
}

/// Iterate through `posts`, advancing `y` by each post's cached height.
/// Skip rows entirely outside the viewport; otherwise call `draw_post_row`.
/// Render the horizontal pill row at the top of TimelineScreen and
/// hit-test pill taps. Returns `Some(idx)` when a non-active pill is
/// tapped (caller switches feed). The pill row is opaque, so it covers
/// any post row that scrolled up into its zone (HEADER_H..HEADER_H+PILL_ROW_H).
///
/// Layout: 6 px left/right outer margin; each pill is at `y =
/// HEADER_H + 7`, h = PILL_ROW_H - 14 = 36. Pill content order: optional
/// 28×28 avatar (Following has none) → 6 px gap → display name in 0.95
/// scale. Active pill has ACCENT background; inactive uses BACKGROUND.
/// Phase 5.1 doesn't horizontally scroll — pills past the right edge are
/// off-screen (and therefore not tappable).
fn draw_pill_row(
    frame: &mut Frame,
    font: &Font,
    pins: &[SavedFeedPin],
    current: &FeedSource,
    pill_buttons: &mut [ButtonState],
    ctx: &UiCtx,
) -> Option<usize> {
    let bar_y = HEADER_H;
    frame.fill_rect(
        0.0,
        bar_y as f32,
        SCREEN_WIDTH as f32,
        PILL_ROW_H as f32,
        theme::BACKGROUND,
    );
    frame.fill_rect(
        0.0,
        (bar_y + PILL_ROW_H - 1) as f32,
        SCREEN_WIDTH as f32,
        1.0,
        theme::TEXT_MUTED,
    );

    let pill_h = PILL_ROW_H - 14;
    let pill_y = bar_y + 7;
    let avatar_size = pill_h - 8; // 28 px, 4 px top/bottom inset
    let mut cx = 6;
    let mut clicked: Option<usize> = None;

    for (idx, pin) in pins.iter().enumerate() {
        let active = pin.source == *current;
        let label_scale = 0.95;
        let (tw, _) = frame.measure_text(font, label_scale, &pin.display_name);
        let has_avatar = pin.avatar_url.is_some();
        let inner_pad_l = if has_avatar { 6 + avatar_size + 6 } else { 12 };
        let pill_w = inner_pad_l + tw + 12;

        let bg_color = if active {
            theme::ACCENT
        } else {
            theme::FIELD_BG
        };
        let text_color = if active {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_MUTED
        };

        frame.fill_rect(cx as f32, pill_y as f32, pill_w as f32, pill_h as f32, bg_color);
        if has_avatar {
            let avatar_x = cx + 6;
            let avatar_y = pill_y + 4;
            let url_thumb = pin.avatar_url.as_deref().map(avatar_thumbnail_jpeg);
            if let Some(url) = url_thumb.as_deref() {
                if let Some(tex) = ctx.texture_cache.get(url) {
                    let sx = avatar_size as f32 / tex.width().max(1) as f32;
                    let sy = avatar_size as f32 / tex.height().max(1) as f32;
                    frame.draw_texture_scale(tex, avatar_x as f32, avatar_y as f32, sx, sy);
                } else {
                    frame.fill_rect(
                        avatar_x as f32,
                        avatar_y as f32,
                        avatar_size as f32,
                        avatar_size as f32,
                        placeholder_color(&pin.display_name),
                    );
                }
            }
        }
        let text_x = cx + inner_pad_l;
        let text_y = pill_y + (pill_h + 18) / 2 - 4;
        frame.draw_text(font, text_x, text_y, text_color, label_scale, &pin.display_name);

        // Hit-test only if pill is on-screen.
        if cx + pill_w <= SCREEN_WIDTH && idx < pill_buttons.len() {
            let pill_rect = Rect::new(
                cx as f32,
                pill_y as f32,
                pill_w as f32,
                pill_h as f32,
            );
            let state = &mut pill_buttons[idx];
            let pressed_now = ctx.touches.iter().any(|t| pill_rect.contains(t.x, t.y));
            let just_clicked =
                state.pressed_last && !pressed_now && ctx.touches.is_empty();
            state.pressed_last = pressed_now;
            if just_clicked && !active {
                clicked = Some(idx);
            }
        }
        cx += pill_w + 6;
    }

    clicked
}

fn draw_post_list(
    frame: &mut Frame,
    font: &Font,
    posts: &[FeedViewPost],
    row_heights: &[i32],
    scroll_y: f32,
    emoji: Option<&EmojiAtlas>,
    cache: &TextureCache,
    avatar_mask: Option<&Texture>,
    avatar_mask_field: Option<&Texture>,
    selected_idx: usize,
) {
    let mut y = VIEWPORT_TOP - scroll_y as i32;
    for (i, (post, &row_h)) in posts.iter().zip(row_heights.iter()).enumerate() {
        let row_bottom = y + row_h;
        if row_bottom > VIEWPORT_TOP && y < VIEWPORT_BOTTOM {
            draw_post_row(
                frame,
                font,
                post,
                y,
                row_h,
                emoji,
                cache,
                avatar_mask,
                avatar_mask_field,
                i == selected_idx,
                true,
            );
        }
        y += row_h;
    }
}

/// Render one post row at the given top-y. The row is positioned in the
/// full screen-width column with `ROW_PAD_X` margin on each side.
/// `is_selected` lights up a left-edge ACCENT bar to indicate keyboard
/// focus.
///
/// Two avatar masks: `avatar_mask` is composited on unselected rows
/// (BACKGROUND-color corners); `avatar_mask_field` on selected rows
/// (FIELD_BG-color corners). The matching corner color makes the
/// circular illusion seamless across both states.
pub(crate) fn draw_post_row(
    frame: &mut Frame,
    font: &Font,
    post: &FeedViewPost,
    y_top: i32,
    row_h: i32,
    emoji: Option<&EmojiAtlas>,
    cache: &TextureCache,
    avatar_mask: Option<&Texture>,
    avatar_mask_field: Option<&Texture>,
    is_selected: bool,
    show_context: bool,
) {
    let row_right = SCREEN_WIDTH;
    let inner_left = TEXT_LEFT;
    let inner_w = row_right - inner_left - ROW_PAD_X;

    // Optional repost/reply context line occupies the top band; the rest
    // of the row (avatar, name, body, embed, counts) shifts down by ctx_h.
    // body_y/counts_y derive from top_y, so shifting top_y cascades them.
    let context_label = post_context_label(post, show_context);
    let ctx_h = if context_label.is_some() { CONTEXT_LINE_H } else { 0 };
    let content_top = y_top + ctx_h;

    // Selection highlight: faint background tint over the row + 3 px
    // ACCENT-color bar on the left edge. The avatar's circular mask
    // has TWO baked variants (BACKGROUND-corner + FIELD_BG-corner) so
    // it composites correctly over both selected and unselected rows.
    if is_selected {
        frame.fill_rect(
            0.0,
            y_top as f32,
            SCREEN_WIDTH as f32,
            row_h as f32,
            theme::FIELD_BG,
        );
        frame.fill_rect(
            0.0,
            y_top as f32,
            3.0,
            row_h as f32,
            theme::ACCENT,
        );
    }

    // Context line ("Reposted by X" / "Reply to @y") in the top band.
    if let Some(label) = &context_label {
        frame.draw_text(font, TEXT_LEFT, y_top + 16, theme::TEXT_MUTED, 0.85, label);
    }

    // Avatar slot: 48×48 in the left margin, top-aligned with text top.
    let avatar_x = ROW_PAD_X;
    let avatar_y = content_top + ROW_PAD_Y;
    let handle_str = post.post.author.handle.as_str();
    let display_str = post
        .post
        .author
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty());
    let mask_for_row = if is_selected {
        avatar_mask_field
    } else {
        avatar_mask
    };
    draw_avatar(
        frame,
        font,
        post.post.author.avatar.as_deref(),
        display_str,
        handle_str,
        avatar_x,
        avatar_y,
        AVATAR_SIZE,
        cache,
        mask_for_row,
    );

    // Top line: display name (left) + @handle (right, muted).
    let display = post
        .post
        .author
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(handle_str);
    let top_y = content_top + ROW_PAD_Y;
    frame.draw_text(font, inner_left, top_y, theme::TEXT_PRIMARY, 1.0, display);

    let handle = format!("@{handle_str}");
    let (hw, _) = frame.measure_text(font, 0.85, &handle);
    let hx = row_right - ROW_PAD_X - hw;
    frame.draw_text(font, hx, top_y + 4, theme::TEXT_MUTED, 0.85, &handle);

    // Body text (wrapped, emoji-aware).
    let body_text = extract_post_text(&post.post.record).unwrap_or_default();
    let body_y = top_y + TOP_LINE_H;
    let body_h = frame.draw_text_wrapped_with_emoji(
        font,
        inner_left,
        body_y,
        inner_w,
        theme::TEXT_PRIMARY,
        1.0,
        &body_text,
        emoji,
    );

    // Embed block (images / link card / quote / video thumb) below body.
    let embed_consumed_h =
        if let Some(embed) = post.post.embed.as_ref() {
            let block = crate::embeds::measure_embed_block(frame, font, Some(embed), emoji);
            if block > 0 {
                crate::embeds::draw_embed_block(
                    frame,
                    font,
                    embed,
                    inner_left,
                    body_y + body_h + crate::embeds::EMBED_GAP,
                    cache,
                    emoji,
                );
            }
            block
        } else {
            0
        };

    // Counts row, three segments rendered in sequence with per-segment
    // color reflecting the viewer's engagement state. Liked / reposted
    // segments render in ACCENT (Bsky-blue); idle in TEXT_MUTED.
    let likes = post.post.like_count.unwrap_or(0);
    let reposts = post.post.repost_count.unwrap_or(0);
    let replies = post.post.reply_count.unwrap_or(0);
    let liked = post
        .post
        .viewer
        .as_ref()
        .and_then(|v| v.like.as_deref())
        .is_some();
    let reposted = post
        .post
        .viewer
        .as_ref()
        .and_then(|v| v.repost.as_deref())
        .is_some();
    let likes_str = format!("{likes} likes");
    let reposts_str = format!("{reposts} reposts");
    let replies_str = format!("{replies} replies");
    let sep_str = "  ·  ";
    let post_embed_gap = if embed_consumed_h > 0 {
        crate::embeds::EMBED_BOTTOM_GAP
    } else {
        BODY_GAP
    };
    let counts_y = body_y + body_h + embed_consumed_h + post_embed_gap;
    let scale = 0.85;
    // Sequential draw: each segment advances current_x by its width
    // (measure_text — chained draw_text return values aren't reliable
    // for next-x; same caveat as draw_word_with_emoji).
    let mut cx = inner_left;
    let likes_color = if liked { theme::ACCENT } else { theme::TEXT_MUTED };
    frame.draw_text(font, cx, counts_y, likes_color, scale, &likes_str);
    cx += frame.measure_text(font, scale, &likes_str).0;
    frame.draw_text(font, cx, counts_y, theme::TEXT_MUTED, scale, sep_str);
    cx += frame.measure_text(font, scale, sep_str).0;
    let reposts_color = if reposted { theme::ACCENT } else { theme::TEXT_MUTED };
    frame.draw_text(font, cx, counts_y, reposts_color, scale, &reposts_str);
    cx += frame.measure_text(font, scale, &reposts_str).0;
    frame.draw_text(font, cx, counts_y, theme::TEXT_MUTED, scale, sep_str);
    cx += frame.measure_text(font, scale, sep_str).0;
    frame.draw_text(font, cx, counts_y, theme::TEXT_MUTED, scale, &replies_str);

    // Separator: 1 px line at the bottom of the row.
    let sep_y = (y_top + row_h - 1) as f32;
    frame.fill_rect(0.0, sep_y, SCREEN_WIDTH as f32, 1.0, theme::FIELD_BG);
}

/// Extract the `text` field from a post's `record: Unknown`. Returns
/// `None` if the record can't be deserialized as
/// `app.bsky.feed.post::Record` (lexicon mismatch, future revisions,
/// or a non-post record served accidentally). For 3.2 we render an
/// empty body in that case; 3.x polish can add a "[unsupported post]"
/// placeholder if we observe it in the wild.
pub(crate) fn extract_post_text(record: &Unknown) -> Option<String> {
    use atrium_api::app::bsky::feed::post::RecordData;
    RecordData::try_from_unknown(record.clone()).ok().map(|r| r.text)
}

/// Render an avatar at `(x, y, size, size)`. Cache hit → scaled
/// texture; cache miss (or no URL) → solid colored placeholder with the
/// first character of the display name (or handle) centered. The
/// optional `avatar_mask` is overlaid on top to fake circular avatars.
fn draw_avatar(
    frame: &mut Frame,
    font: &Font,
    url: Option<&str>,
    display_name: Option<&str>,
    handle: &str,
    x: i32,
    y: i32,
    size: i32,
    cache: &TextureCache,
    avatar_mask: Option<&Texture>,
) {
    let mut painted_real = false;
    if let Some(url) = url {
        // Cache key matches the dispatch URL — both use the
        // thumbnail-JPEG variant.
        let url = avatar_thumbnail_jpeg(url);
        if let Some(tex) = cache.get(&url) {
            let sx = size as f32 / tex.width().max(1) as f32;
            let sy = size as f32 / tex.height().max(1) as f32;
            frame.draw_texture_scale(tex, x as f32, y as f32, sx, sy);
            painted_real = true;
        }
    }
    if !painted_real {
        // Placeholder: colored square + initial letter.
        frame.fill_rect(
            x as f32,
            y as f32,
            size as f32,
            size as f32,
            placeholder_color(handle),
        );
        draw_avatar_initial(frame, font, display_name, handle, x, y, size);
    }
    // Mask: opaque-corners + transparent-disk overlay → circular look.
    if let Some(mask) = avatar_mask {
        let sx = size as f32 / mask.width().max(1) as f32;
        let sy = size as f32 / mask.height().max(1) as f32;
        frame.draw_texture_scale(mask, x as f32, y as f32, sx, sy);
    }
}

/// Draw a single uppercase letter from `display_name` (or `handle` if
/// no display name) centered inside the avatar slot. Used as the
/// fallback avatar's "monogram" while the cache miss is in flight or
/// the user has no avatar uploaded.
fn draw_avatar_initial(
    frame: &mut Frame,
    font: &Font,
    display_name: Option<&str>,
    handle: &str,
    x: i32,
    y: i32,
    size: i32,
) {
    let source = display_name.unwrap_or(handle);
    let initial = source
        .chars()
        .next()
        .unwrap_or('?')
        .to_ascii_uppercase()
        .to_string();
    // Scale chosen so the letter takes up roughly half the avatar slot
    // height (size 48 → ~24 px letter; size 96 → ~48 px). Anchored on
    // base size 20 (Inter pixel size at scale 1.0).
    let scale = (size as f32) / 40.0;
    let (tw, th) = frame.measure_text(font, scale, &initial);
    let tx = x + (size - tw) / 2;
    // Approximate vertical centering: baseline at center + th/2 (since
    // font measurement reports total height including descender).
    let ty = y + (size + th) / 2 - 4;
    frame.draw_text(font, tx, ty, theme::BACKGROUND, scale, &initial);
}

/// Pick a stable pastel color for an account based on its handle. 8-color
/// palette; same handle always gets the same color.
fn placeholder_color(handle: &str) -> Color {
    const PALETTE: [Color; 8] = [
        Color::rgb(0xF8, 0x9A, 0x9A), // pink
        Color::rgb(0xF8, 0xC1, 0x9A), // peach
        Color::rgb(0xF8, 0xE8, 0x9A), // yellow
        Color::rgb(0x9A, 0xF8, 0xA0), // mint
        Color::rgb(0x9A, 0xE0, 0xF8), // sky
        Color::rgb(0x9A, 0xA0, 0xF8), // periwinkle
        Color::rgb(0xC4, 0x9A, 0xF8), // lavender
        Color::rgb(0xF8, 0x9A, 0xE0), // rose
    ];
    // Cheap FNV-like hash; sufficient for 8-bucket palette dispersion.
    let mut h: u32 = 2166136261;
    for b in handle.bytes() {
        h = h.wrapping_mul(16777619) ^ b as u32;
    }
    PALETTE[(h as usize) % PALETTE.len()]
}

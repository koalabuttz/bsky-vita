//! Thread screen — single-post view with parent context + replies.
//!
//! Pushed onto the navigation stack from TimelineScreen when the user
//! taps a post body (or any visible post not in the author / counts
//! tap zones). Fetches the thread via `WorkRequest::FetchThread` and
//! renders three blocks vertically:
//!
//! 1. **Parents** — ancestors of the focus, oldest first. Visually
//!    de-emphasized (small left indent).
//! 2. **Main** — the post the user tapped on. Highlighted with a
//!    thicker ACCENT-color left bar.
//! 3. **Replies** — direct replies to main. Phase 4.4 MVP: first-level
//!    only; tapping a reply opens a new ThreadScreen for that reply
//!    (recursion via the screen stack).
//!
//! Selection model + L/R/TRIANGLE bindings mirror TimelineScreen, but
//! act on the focused post within the flattened parents+main+replies
//! list. Reply (`R`) builds a `ReplyTarget` with the *correct* root
//! URI (parents[0] if any, else main) — fixing the 4.2 MVP simplification
//! that used parent-as-root for direct replies in TimelineScreen.

use std::collections::HashSet;
use std::sync::Arc;

use atrium_api::app::bsky::feed::defs::{FeedViewPost, FeedViewPostData, PostView};
use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_input::buttons;
use bsky_render::{theme, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{ReplyTarget, WorkRequest, WorkResponse};

use crate::cdn::avatar_thumbnail_jpeg;
use crate::compose::ComposeScreen;
use crate::profile::ProfileScreen;
use crate::screen::{Screen, ScreenAction};
use crate::timeline::{
    self, draw_post_row, measure_post_row, EngagementKind, AVATAR_SIZE, ROW_PAD_X, ROW_PAD_Y,
    TEXT_LEFT,
};
use crate::widget::Rect;

const HEADER_H: i32 = 40;

enum ThreadState {
    Loading,
    Loaded {
        /// Flattened post list: parents (oldest first), then main, then
        /// replies. `main_idx` points at the main post.
        posts: Vec<FeedViewPost>,
        main_idx: usize,
    },
    Error(String),
}

pub struct ThreadScreen {
    client: Arc<AuthClient>,
    /// AT-URI of the post the user originally tapped on. The thread
    /// fetch can return a different "main" if depth/parent_height
    /// chooses a higher ancestor; for 4.4 we always treat the response
    /// `main` (the focus of the recursive structure) as the main row.
    uri: String,
    state: ThreadState,
    scroll_y: f32,
    selected_idx: usize,
    dispatched: bool,
    row_heights: Vec<i32>,
    inflight_avatars: HashSet<String>,
    /// Set on Thread response. Triggers a one-shot scroll-to-main on
    /// the first frame after row_heights are measured (otherwise the
    /// thread renders with scroll_y = 0, which puts the main post off
    /// the bottom when there are many parents).
    pending_scroll_to_main: bool,
}

impl ThreadScreen {
    pub fn new(client: Arc<AuthClient>, uri: String) -> Self {
        Self {
            client,
            uri,
            state: ThreadState::Loading,
            scroll_y: 0.0,
            selected_idx: 0,
            dispatched: false,
            row_heights: Vec::new(),
            inflight_avatars: HashSet::new(),
            pending_scroll_to_main: false,
        }
    }

    fn focused_reply_target(&self) -> Option<ReplyTarget> {
        let posts = match &self.state {
            ThreadState::Loaded { posts, .. } => posts,
            _ => return None,
        };
        let parent = posts.get(self.selected_idx)?;
        let parent_uri = parent.post.uri.clone();
        let parent_cid = parent.post.cid.as_ref().to_string();
        // Root = first post in the flattened list (the thread's
        // topmost ancestor). If selected post is the root, root = parent.
        let root = posts.first()?;
        let root_uri = root.post.uri.clone();
        let root_cid = root.post.cid.as_ref().to_string();
        Some(ReplyTarget {
            parent_uri,
            parent_cid,
            root_uri,
            root_cid,
        })
    }

    fn toggle_engagement(&mut self, ctx: &crate::widget::UiCtx, kind: EngagementKind) {
        let Some(worker) = ctx.worker else { return };
        let ThreadState::Loaded { posts, .. } = &mut self.state else { return };
        let Some(post) = posts.get_mut(self.selected_idx) else { return };
        timeline::toggle_engagement(post, worker, kind);
    }
}

impl Screen for ThreadScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &crate::widget::UiCtx,
        _ime: &mut Ime,
    ) -> ScreenAction {
        // First-frame fetch.
        if !self.dispatched {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::FetchThread {
                    uri: self.uri.clone(),
                });
                self.dispatched = true;
            }
        }

        // Input.
        if ctx.pad.just_pressed(buttons::CIRCLE) {
            return ScreenAction::Pop;
        }
        let post_count = match &self.state {
            ThreadState::Loaded { posts, .. } => posts.len(),
            _ => 0,
        };
        let mut selection_changed = false;
        if post_count > 0
            && (ctx.pad.just_pressed(buttons::UP) || ctx.pad.just_pressed(buttons::DOWN))
        {
            if ctx.pad.just_pressed(buttons::UP) && self.selected_idx > 0 {
                self.selected_idx -= 1;
                selection_changed = true;
            }
            if ctx.pad.just_pressed(buttons::DOWN) && self.selected_idx + 1 < post_count {
                self.selected_idx += 1;
                selection_changed = true;
            }
        }
        if ctx.pad.just_pressed(buttons::L1) {
            self.toggle_engagement(ctx, EngagementKind::Like);
        }
        if ctx.pad.just_pressed(buttons::TRIANGLE) {
            self.toggle_engagement(ctx, EngagementKind::Repost);
        }
        if ctx.pad.just_pressed(buttons::R1) {
            if let Some(reply) = self.focused_reply_target() {
                let handle = match &self.state {
                    ThreadState::Loaded { posts, .. } => posts
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

        // Lazy-measure row heights.
        if let ThreadState::Loaded { posts, .. } = &self.state {
            while self.row_heights.len() < posts.len() {
                let i = self.row_heights.len();
                let h = measure_post_row(frame, font, &posts[i], ctx.emoji);
                self.row_heights.push(h);
            }
        }

        // Auto-scroll on selection change to keep focused row visible.
        let total_h: i32 = self.row_heights.iter().sum();
        let viewport_h = SCREEN_HEIGHT - HEADER_H;
        let max_scroll = (total_h - viewport_h).max(0) as f32;

        // One-shot scroll-to-main after the response lands and row
        // heights are populated. Centers the main post in the viewport
        // (or as close as the scroll bounds allow).
        if self.pending_scroll_to_main
            && self.selected_idx < self.row_heights.len()
            && !self.row_heights.is_empty()
        {
            let row_top: i32 = self.row_heights[..self.selected_idx].iter().sum();
            let row_h = self.row_heights[self.selected_idx];
            let target = (row_top + row_h / 2 - viewport_h / 2).max(0) as f32;
            self.scroll_y = target.min(max_scroll);
            self.pending_scroll_to_main = false;
        }

        if selection_changed && self.selected_idx < self.row_heights.len() {
            let row_top: i32 = self.row_heights[..self.selected_idx].iter().sum();
            let row_h = self.row_heights[self.selected_idx];
            let view_top = self.scroll_y as i32;
            let view_bottom = view_top + viewport_h;
            const SCROLL_MARGIN: i32 = 50;
            if row_top < view_top + SCROLL_MARGIN {
                self.scroll_y = (row_top - SCROLL_MARGIN).max(0) as f32;
            } else if row_top + row_h > view_bottom - SCROLL_MARGIN {
                self.scroll_y =
                    (row_top + row_h + SCROLL_MARGIN - viewport_h).max(0) as f32;
            }
        }
        // Analog stick free-scroll.
        let stick_y = ctx.pad.left_stick.1;
        let mag = stick_y.unsigned_abs() as f32;
        const STICK_DEADZONE: f32 = 32.0;
        if mag > STICK_DEADZONE {
            let sign: f32 = if stick_y < 0 { -1.0 } else { 1.0 };
            let effective = (mag - STICK_DEADZONE) * sign;
            self.scroll_y += effective / 24.0;
        }
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);

        // Tap detection: author region → Push ProfileScreen for that
        // actor. Same hit-test pattern as TimelineScreen but operating
        // on the parent + main + replies list. Accumulates a tap
        // result outside the immutable post borrow, then applies after.
        if !ctx.touches.is_empty() {
            let touches: Vec<_> = ctx.touches.iter().map(|t| (t.x, t.y)).collect();
            let mut author_tap: Option<String> = None;
            if let ThreadState::Loaded { posts, .. } = &self.state {
                let mut y_probe = HEADER_H - self.scroll_y as i32;
                for (post, &row_h) in posts.iter().zip(self.row_heights.iter()) {
                    let row_bottom = y_probe + row_h;
                    if row_bottom > HEADER_H && y_probe < SCREEN_HEIGHT {
                        let author_rect = Rect::new(
                            0.0,
                            (y_probe + ROW_PAD_Y) as f32,
                            TEXT_LEFT as f32,
                            AVATAR_SIZE as f32,
                        );
                        if touches.iter().any(|&(x, y)| author_rect.contains(x, y)) {
                            author_tap =
                                Some(post.post.author.handle.as_str().to_string());
                            break;
                        }
                    }
                    y_probe += row_h;
                }
            }
            if let Some(handle) = author_tap {
                return ScreenAction::Push(Box::new(ProfileScreen::new(
                    Arc::clone(&self.client),
                    Some(handle),
                )));
            }
        }
        // Suppress unused-import lint when ROW_PAD_X is referenced only
        // indirectly via TEXT_LEFT.
        let _ = ROW_PAD_X;

        // Avatar dispatch for visible posts.
        if let ThreadState::Loaded { posts, .. } = &self.state {
            if let Some(worker) = ctx.worker {
                let mut y_probe = HEADER_H - self.scroll_y as i32;
                for (post, &row_h) in posts.iter().zip(self.row_heights.iter()) {
                    let row_bottom = y_probe + row_h;
                    if row_bottom > HEADER_H && y_probe < SCREEN_HEIGHT {
                        if let Some(url) = post.post.author.avatar.as_ref() {
                            let url = avatar_thumbnail_jpeg(url);
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

        // Render.
        match &self.state {
            ThreadState::Loading => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2,
                    theme::TEXT_MUTED,
                    1.1,
                    "Loading thread…",
                );
            }
            ThreadState::Loaded { posts, main_idx } => {
                let mut y = HEADER_H - self.scroll_y as i32;
                for (i, (post, &row_h)) in
                    posts.iter().zip(self.row_heights.iter()).enumerate()
                {
                    let row_bottom = y + row_h;
                    if row_bottom > HEADER_H && y < SCREEN_HEIGHT {
                        // Highlight the main post with an ACCENT bar
                        // wider than the regular selection bar.
                        if i == *main_idx {
                            frame.fill_rect(
                                0.0,
                                y as f32,
                                6.0,
                                row_h as f32,
                                theme::ACCENT,
                            );
                        }
                        draw_post_row(
                            frame,
                            font,
                            post,
                            y,
                            row_h,
                            ctx.emoji,
                            ctx.texture_cache,
                            ctx.avatar_mask,
                            ctx.avatar_mask_field,
                            i == self.selected_idx,
                        );
                    }
                    y += row_h;
                }
            }
            ThreadState::Error(msg) => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 - 20,
                    theme::ERROR,
                    1.0,
                    "Could not load thread",
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

        // Sticky header (drawn last to cover scrolled content).
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, HEADER_H as f32, theme::FIELD_BG);
        frame.draw_text_centered(font, 26, theme::TEXT_PRIMARY, 1.1, "Thread");
        frame.fill_rect(
            0.0,
            HEADER_H as f32 - 1.0,
            SCREEN_WIDTH as f32,
            1.0,
            theme::TEXT_MUTED,
        );

        ScreenAction::None
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        match resp {
            WorkResponse::Thread(Ok(batch)) => {
                let main_idx = batch.parents.len();
                let mut posts: Vec<FeedViewPost> = batch
                    .parents
                    .into_iter()
                    .chain(std::iter::once(batch.main.clone()))
                    .chain(batch.replies.into_iter())
                    .map(post_to_feed)
                    .collect();
                // Default selection lands on the main post the user
                // tapped on (not the topmost ancestor).
                self.selected_idx = main_idx.min(posts.len().saturating_sub(1));
                // Rebuild row_heights on next frame (cleared here so
                // the lazy-measure loop re-runs).
                self.row_heights.clear();
                self.pending_scroll_to_main = true;
                self.state = ThreadState::Loaded { posts: std::mem::take(&mut posts), main_idx };
            }
            WorkResponse::Thread(Err(e)) => {
                self.state = ThreadState::Error(e);
            }
            // Live engagement responses from in-flight L/TRIANGLE on this
            // thread: confirm in-place URI replacement (PENDING_URI sentinel).
            WorkResponse::LikeChanged(Ok(Some(uri))) => {
                if let ThreadState::Loaded { posts, .. } = &mut self.state {
                    for post in posts.iter_mut() {
                        if let Some(viewer) = post.post.viewer.as_mut() {
                            if viewer.like.as_deref() == Some(timeline::PENDING_URI) {
                                viewer.like = Some(uri);
                                break;
                            }
                        }
                    }
                }
            }
            WorkResponse::RepostChanged(Ok(Some(uri))) => {
                if let ThreadState::Loaded { posts, .. } = &mut self.state {
                    for post in posts.iter_mut() {
                        if let Some(viewer) = post.post.viewer.as_mut() {
                            if viewer.repost.as_deref() == Some(timeline::PENDING_URI) {
                                viewer.repost = Some(uri);
                                break;
                            }
                        }
                    }
                }
            }
            WorkResponse::Image { url, .. } => {
                self.inflight_avatars.remove(&url);
            }
            // Everything else (Profile, FollowChanged, PostCreated,
            // delete-acks, errors) is not for us.
            _ => {}
        }
    }
}

/// Wrap a bare `PostView` in a synthetic `FeedViewPost` so we can call
/// the shared `draw_post_row` / `measure_post_row` helpers (which
/// expect FeedViewPost). reason / reply / feed_context / req_id are
/// `None` since we don't have those here.
fn post_to_feed(post: PostView) -> FeedViewPost {
    FeedViewPostData {
        post,
        reason: None,
        reply: None,
        feed_context: None,
        req_id: None,
    }
    .into()
}

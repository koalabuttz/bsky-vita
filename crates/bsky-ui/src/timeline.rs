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

use atrium_api::app::bsky::feed::defs::FeedViewPost;
use atrium_api::types::{TryFromUnknown, Unknown};
use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_input::buttons;
use bsky_render::{
    theme, Color, EmojiAtlas, Font, Frame, Texture, TextureCache, SCREEN_HEIGHT, SCREEN_WIDTH,
};
use bsky_worker::{WorkRequest, WorkResponse};

use crate::cdn::avatar_thumbnail_jpeg;
use crate::profile::ProfileScreen;
use crate::screen::{Screen, ScreenAction};
use crate::widget::UiCtx;

/// Sticky header height (px). Posts render below this; the header is
/// drawn last so it covers any content that's scrolled into its zone.
const HEADER_H: i32 = 40;

/// Viewport for the post list (everything below the header).
const VIEWPORT_TOP: i32 = HEADER_H;
const VIEWPORT_H: i32 = SCREEN_HEIGHT - HEADER_H;

/// Side margins inside a post row.
const ROW_PAD_X: i32 = 16;
/// Avatar dimensions in the timeline post row.
const AVATAR_SIZE: i32 = 48;
/// Where text starts inside a row, accounting for the avatar slot:
/// ROW_PAD_X (left margin) + AVATAR_SIZE + 8 px gutter.
const TEXT_LEFT: i32 = ROW_PAD_X + AVATAR_SIZE + 8;
/// Vertical padding inside a row (above display name, below counts).
/// 20 px clears the ascender of 20 px text (ascender ~15–16 px above
/// baseline), preventing letter tops from overlapping the previous
/// row's separator. Was 12 in the PGF era.
const ROW_PAD_Y: i32 = 20;
/// Gap between display-name row and post body.
const TOP_LINE_H: i32 = 24;
/// Gap between post body and counts row.
const BODY_GAP: i32 = 8;
/// Counts row height + bottom padding + separator gap.
const FOOTER_H: i32 = 28;

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
    /// Scroll offset in pixels from the top of the post list (0 = first
    /// post at y=HEADER_H).
    scroll_y: f32,
    /// Have we sent the initial FetchTimeline yet?
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
}

impl TimelineScreen {
    pub fn new(client: Arc<AuthClient>) -> Self {
        Self {
            client,
            state: TimelineState::Loading,
            scroll_y: 0.0,
            dispatched: false,
            fetching_more: false,
            row_heights: Vec::new(),
            inflight_avatars: HashSet::new(),
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
                worker.send(WorkRequest::FetchTimeline { cursor: None });
                self.dispatched = true;
            }
        }

        // ─── 2. Input: scroll + back. ──────────────────────────────────
        if ctx.pad.just_pressed(buttons::CIRCLE) {
            return ScreenAction::Goto(Box::new(ProfileScreen::new(
                Arc::clone(&self.client),
            )));
        }
        if ctx.pad.just_pressed(buttons::UP) {
            self.scroll_y -= DPAD_STEP;
        }
        if ctx.pad.just_pressed(buttons::DOWN) {
            self.scroll_y += DPAD_STEP;
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

        // ─── 3. Lazy-measure row heights for any newly-arrived posts. ─
        if let TimelineState::Loaded { posts, .. } = &self.state {
            while self.row_heights.len() < posts.len() {
                let i = self.row_heights.len();
                let h = measure_post_row(frame, font, &posts[i], ctx.emoji);
                self.row_heights.push(h);
            }
        }

        // ─── 4. Compute layout: total content height + scroll clamp. ──
        let total_h: i32 = self.row_heights.iter().sum();
        let max_scroll = (total_h - VIEWPORT_H).max(0) as f32;
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
                        worker.send(WorkRequest::FetchTimeline {
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
                let mut y_probe = HEADER_H - self.scroll_y as i32;
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
                    }
                    y_probe += row_h;
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
                    );
                    if self.fetching_more {
                        let bottom_y = HEADER_H + total_h - self.scroll_y as i32 + 8;
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
                        let bottom_y = HEADER_H + total_h - self.scroll_y as i32 + 8;
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

        // ─── 7. Sticky header (drawn last, on top of any post row that
        //        scrolled up into the header zone). ───────────────────
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, HEADER_H as f32, theme::FIELD_BG);
        frame.draw_text_centered(font, 26, theme::TEXT_PRIMARY, 1.1, "Following");
        // Bottom edge of header (1px separator).
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
            WorkResponse::Timeline(Ok(batch)) => {
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
            WorkResponse::Timeline(Err(e)) => {
                self.fetching_more = false;
                if matches!(self.state, TimelineState::Loading) {
                    self.state = TimelineState::Error(e);
                }
                // Page-load failures: silently ignored. The user can scroll
                // to retrigger; the cursor is unchanged so the next attempt
                // starts from the same point.
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
        }
    }
}

/// Compute one post row's total height (without drawing). Mirrors the
/// layout in [`draw_post_row`].
fn measure_post_row(
    frame: &Frame,
    font: &Font,
    post: &FeedViewPost,
    emoji: Option<&EmojiAtlas>,
) -> i32 {
    // Body text wraps to the column right of the avatar slot.
    let inner_w = SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X;
    let body_text = extract_post_text(&post.post.record).unwrap_or_default();
    let body_h = frame.measure_text_wrapped_with_emoji(font, inner_w, 1.0, &body_text, emoji);
    let text_block_h = ROW_PAD_Y + TOP_LINE_H + body_h + BODY_GAP + FOOTER_H;
    // Ensure the row is at least as tall as the avatar slot.
    let avatar_block_h = ROW_PAD_Y + AVATAR_SIZE + FOOTER_H;
    text_block_h.max(avatar_block_h)
}

/// Iterate through `posts`, advancing `y` by each post's cached height.
/// Skip rows entirely outside the viewport; otherwise call `draw_post_row`.
fn draw_post_list(
    frame: &mut Frame,
    font: &Font,
    posts: &[FeedViewPost],
    row_heights: &[i32],
    scroll_y: f32,
    emoji: Option<&EmojiAtlas>,
    cache: &TextureCache,
    avatar_mask: Option<&Texture>,
) {
    let mut y = HEADER_H - scroll_y as i32;
    for (post, &row_h) in posts.iter().zip(row_heights.iter()) {
        let row_bottom = y + row_h;
        if row_bottom > VIEWPORT_TOP && y < SCREEN_HEIGHT {
            draw_post_row(frame, font, post, y, row_h, emoji, cache, avatar_mask);
        }
        y += row_h;
    }
}

/// Render one post row at the given top-y. The row is positioned in the
/// full screen-width column with `ROW_PAD_X` margin on each side.
fn draw_post_row(
    frame: &mut Frame,
    font: &Font,
    post: &FeedViewPost,
    y_top: i32,
    row_h: i32,
    emoji: Option<&EmojiAtlas>,
    cache: &TextureCache,
    avatar_mask: Option<&Texture>,
) {
    let row_right = SCREEN_WIDTH;
    let inner_left = TEXT_LEFT;
    let inner_w = row_right - inner_left - ROW_PAD_X;

    // Avatar slot: 48×48 in the left margin, top-aligned with text top.
    let avatar_x = ROW_PAD_X;
    let avatar_y = y_top + ROW_PAD_Y;
    let handle_str = post.post.author.handle.as_str();
    let display_str = post
        .post
        .author
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty());
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
        avatar_mask,
    );

    // Top line: display name (left) + @handle (right, muted).
    let display = post
        .post
        .author
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(handle_str);
    let top_y = y_top + ROW_PAD_Y;
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

    // Counts row.
    let likes = post.post.like_count.unwrap_or(0);
    let reposts = post.post.repost_count.unwrap_or(0);
    let replies = post.post.reply_count.unwrap_or(0);
    let counts = format!(
        "{likes} likes  ·  {reposts} reposts  ·  {replies} replies"
    );
    let counts_y = body_y + body_h + BODY_GAP;
    frame.draw_text(font, inner_left, counts_y, theme::TEXT_MUTED, 0.85, &counts);

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
fn extract_post_text(record: &Unknown) -> Option<String> {
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

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

use atrium_api::app::bsky::actor::defs::ProfileViewDetailedData;
use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_render::{theme, Color, Font, Frame, Texture, TextureCache, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{WorkRequest, WorkResponse};

use crate::cdn::avatar_thumbnail_jpeg;
use crate::screen::{Screen, ScreenAction};
use crate::tabbar::{TabBar, TopLevel};
use crate::widget::{button, ButtonState, Rect, UiCtx};

const AVATAR_SIZE: i32 = 96;

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
    /// Tab bar (only rendered for the own-profile / top-level instance;
    /// pushed-sub-screen instances with `actor: Some(_)` skip rendering
    /// it because they're below the tab bar in the navigation stack).
    tab_bar: TabBar,
    /// Tap state for the Follow / Unfollow button (rendered only when
    /// `actor.is_some()`, i.e. viewing somebody else's profile).
    follow_btn: ButtonState,
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
            tab_bar: TabBar::new(TopLevel::Profile),
            follow_btn: ButtonState::default(),
        }
    }

    /// True if this is the user's own-profile (top-level) instance.
    fn is_own(&self) -> bool {
        self.actor.is_none()
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

        // Title bar (consistent with LoginScreen).
        frame.draw_text_centered(font, 40, theme::TEXT_PRIMARY, 1.6, "bsky-vita");

        // Dispatch avatar fetch if we have the URL and it isn't cached
        // / already in flight. Transform to thumbnail-JPEG so the cache
        // lookup matches the eventual dispatch URL.
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
        }

        let mut toggle_follow_clicked = false;
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
                draw_profile(frame, font, p, ctx.texture_cache, ctx.avatar_mask);
                if self.is_own() {
                    draw_session_footer(frame, font, &self.client);
                } else {
                    // Follow / Unfollow button on other actors' profiles.
                    let following = p
                        .viewer
                        .as_ref()
                        .and_then(|v| v.following.as_deref())
                        .is_some();
                    let label = if following { "Unfollow" } else { "Follow" };
                    let btn_w = 160.0;
                    let btn_rect = Rect::new(
                        (SCREEN_WIDTH as f32 - btn_w) / 2.0,
                        395.0,
                        btn_w,
                        48.0,
                    );
                    let clicked = button(
                        frame,
                        font,
                        btn_rect,
                        label,
                        &mut self.follow_btn,
                        ctx,
                        true,
                    );
                    if clicked {
                        toggle_follow_clicked = true;
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
            WorkResponse::Profile(Ok(p)) => self.state = ProfileState::Loaded(p),
            WorkResponse::Profile(Err(e)) => self.state = ProfileState::Error(e),
            // Timeline responses can arrive after the user navigated back
            // from TimelineScreen mid-fetch. Drop them.
            WorkResponse::Timeline(_) => {}
            // Image responses: cache is updated in main.rs; we just clear
            // our in-flight tracker so a future cache-miss can re-dispatch.
            WorkResponse::Image { url, .. } => {
                if self.inflight_avatar.as_deref() == Some(&url) {
                    self.inflight_avatar = None;
                }
            }
            WorkResponse::PostCreated(_) => {}
            WorkResponse::LikeChanged(_) | WorkResponse::RepostChanged(_) => {}
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
        }
    }
}

fn draw_profile(
    frame: &mut Frame,
    font: &Font,
    p: &ProfileViewDetailedData,
    cache: &TextureCache,
    avatar_mask: Option<&Texture>,
) {
    // Avatar slot: 96×96 centered horizontally below the title.
    let avatar_x = (SCREEN_WIDTH - AVATAR_SIZE) / 2;
    let avatar_y = 72;
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
        // Placeholder: colored square + initial.
        frame.fill_rect(
            avatar_x as f32,
            avatar_y as f32,
            AVATAR_SIZE as f32,
            AVATAR_SIZE as f32,
            placeholder_color(p.handle.as_str()),
        );
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
    // Circular-mask overlay (corners → BACKGROUND).
    if let Some(mask) = avatar_mask {
        let sx = AVATAR_SIZE as f32 / mask.width().max(1) as f32;
        let sy = AVATAR_SIZE as f32 / mask.height().max(1) as f32;
        frame.draw_texture_scale(mask, avatar_x as f32, avatar_y as f32, sx, sy);
    }

    // Display name (or fallback to handle). Shifted down to make room
    // for the avatar above.
    let display = p
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p.handle.as_str());
    frame.draw_text_centered(font, 200, theme::TEXT_PRIMARY, 2.2, display);

    // @handle
    let handle = format!("@{}", p.handle.as_str());
    frame.draw_text_centered(font, 240, theme::TEXT_MUTED, 1.1, &handle);

    // Description (multi-line wrapped, emoji-aware). Constrained to a
    // 4-line height budget so the counts row below stays in place; the
    // 4th line truncates with an ellipsis if there's more.
    if let Some(desc) = p.description.as_deref().filter(|s| !s.is_empty()) {
        const DESC_MAX_W: i32 = SCREEN_WIDTH - 80;
        const DESC_MAX_LINES: usize = 4;
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
            290,
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
    frame.draw_text_centered(font, 345, theme::TEXT_MUTED, 1.0, &line);
}

/// Mirrors `timeline.rs::placeholder_color`. Stable pastel color per
/// handle. Inlined here to avoid a public re-export from timeline.
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

fn draw_session_footer(frame: &mut Frame, font: &Font, client: &AuthClient) {
    let did_line = format!("did: {}", client.resolved.did);
    let pds_line = format!("pds: {}", client.resolved.pds);
    let _ = SCREEN_WIDTH;
    frame.draw_text_centered(
        font,
        SCREEN_HEIGHT - 50,
        theme::TEXT_MUTED,
        0.7,
        &did_line,
    );
    frame.draw_text_centered(
        font,
        SCREEN_HEIGHT - 25,
        theme::TEXT_MUTED,
        0.7,
        &pds_line,
    );
}

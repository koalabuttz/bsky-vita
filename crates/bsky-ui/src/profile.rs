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
use crate::timeline::TimelineScreen;
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
    state: ProfileState,
    /// Tracks whether we've already sent the `GetOwnProfile` request this
    /// session. Without this we'd re-dispatch every frame while the
    /// response is in flight.
    dispatched: bool,
    timeline_btn: ButtonState,
    /// Avatar URL we've dispatched a fetch for; suppresses re-dispatch
    /// while in flight. Cleared on `WorkResponse::Image`.
    inflight_avatar: Option<String>,
}

impl ProfileScreen {
    pub fn new(client: Arc<AuthClient>) -> Self {
        Self {
            client,
            state: ProfileState::Pending,
            dispatched: false,
            timeline_btn: ButtonState::default(),
            inflight_avatar: None,
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
                worker.send(WorkRequest::GetOwnProfile);
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

        let mut timeline_clicked = false;
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
                let btn_w = 160.0;
                let btn_rect = Rect::new(
                    (SCREEN_WIDTH as f32 - btn_w) / 2.0,
                    395.0,
                    btn_w,
                    48.0,
                );
                timeline_clicked = button(
                    frame,
                    font,
                    btn_rect,
                    "Timeline",
                    &mut self.timeline_btn,
                    ctx,
                    true,
                );
                draw_session_footer(frame, font, &self.client);
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

        if timeline_clicked {
            return ScreenAction::Goto(Box::new(TimelineScreen::new(Arc::clone(
                &self.client,
            ))));
        }

        ScreenAction::None
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

    // Description (truncated, single line).
    if let Some(desc) = p.description.as_deref().filter(|s| !s.is_empty()) {
        let head = desc.chars().take(80).collect::<String>();
        let line = if desc.chars().count() > 80 {
            format!("{head}…")
        } else {
            head
        };
        frame.draw_text_centered(font, 290, theme::TEXT_PRIMARY, 0.95, &line);
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

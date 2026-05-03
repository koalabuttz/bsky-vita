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
use bsky_render::{theme, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{WorkRequest, WorkResponse};

use crate::screen::{Screen, ScreenAction};
use crate::timeline::TimelineScreen;
use crate::widget::{button, ButtonState, Rect, UiCtx};

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
}

impl ProfileScreen {
    pub fn new(client: Arc<AuthClient>) -> Self {
        Self {
            client,
            state: ProfileState::Pending,
            dispatched: false,
            timeline_btn: ButtonState::default(),
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
                draw_profile(frame, font, p);
                let btn_w = 160.0;
                let btn_rect = Rect::new(
                    (SCREEN_WIDTH as f32 - btn_w) / 2.0,
                    350.0,
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
        }
    }
}

fn draw_profile(frame: &mut Frame, font: &Font, p: &ProfileViewDetailedData) {
    // Display name (or fallback to handle).
    let display = p
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p.handle.as_str());
    frame.draw_text_centered(font, 110, theme::TEXT_PRIMARY, 2.2, display);

    // @handle
    let handle = format!("@{}", p.handle.as_str());
    frame.draw_text_centered(font, 160, theme::TEXT_MUTED, 1.1, &handle);

    // Description (truncated). Multi-line not supported by our PGF wrapper
    // yet — draw the first ~80 chars only.
    if let Some(desc) = p.description.as_deref().filter(|s| !s.is_empty()) {
        let head = desc.chars().take(80).collect::<String>();
        let line = if desc.chars().count() > 80 {
            format!("{head}…")
        } else {
            head
        };
        frame.draw_text_centered(font, 220, theme::TEXT_PRIMARY, 0.95, &line);
    }

    // Counts row: posts | followers | following.
    let posts = p.posts_count.unwrap_or(0);
    let followers = p.followers_count.unwrap_or(0);
    let follows = p.follows_count.unwrap_or(0);
    let line = format!(
        "{posts} posts     {followers} followers     {follows} following"
    );
    frame.draw_text_centered(font, 290, theme::TEXT_MUTED, 1.0, &line);
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

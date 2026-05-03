//! Profile screen — shows the logged-in user's own profile.
//!
//! Two-frame load pattern:
//! 1. Frame N: `frame()` renders "Loading profile…" while state is `Pending`.
//! 2. After present: `after_present()` blocks on `getProfile`.
//! 3. Frame N+1: `frame()` renders display name + handle + counts.
//!
//! No logout / no refresh actions in 2.5 — Phase 3 polish.

use atrium_api::app::bsky::actor::defs::ProfileViewDetailedData;
use atrium_api::app::bsky::actor::get_profile;
use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_render::{theme, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};
use futures::executor::block_on;

use crate::screen::{Screen, ScreenAction};
use crate::widget::UiCtx;

enum ProfileState {
    /// First frame — render loading; `after_present` will fire the call.
    Pending,
    /// `getProfile` returned successfully.
    Loaded(Box<ProfileViewDetailedData>),
    /// `getProfile` failed.
    Error(String),
}

pub struct ProfileScreen {
    client: AuthClient,
    state: ProfileState,
}

impl ProfileScreen {
    pub fn new(client: AuthClient) -> Self {
        Self {
            client,
            state: ProfileState::Pending,
        }
    }
}

impl Screen for ProfileScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        _ctx: &UiCtx,
        _ime: &mut Ime,
    ) -> ScreenAction {
        // Title bar (consistent with LoginScreen).
        frame.draw_text_centered(font, 40, theme::TEXT_PRIMARY, 1.6, "bsky-vita");

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

        ScreenAction::None
    }

    fn after_present(&mut self) {
        if matches!(self.state, ProfileState::Pending) {
            // Use the user's own DID for the actor param.
            let did = match block_on(self.client.agent.did()) {
                Some(d) => d,
                None => {
                    self.state =
                        ProfileState::Error("agent has no session DID — unexpected".into());
                    return;
                }
            };
            let result = block_on(self.client.agent.api.app.bsky.actor.get_profile(
                get_profile::ParametersData {
                    actor: did.into(),
                }
                .into(),
            ));
            match result {
                Ok(p) => self.state = ProfileState::Loaded(Box::new(p.data)),
                Err(e) => self.state = ProfileState::Error(format!("{e}")),
            }
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

fn draw_session_footer(frame: &mut Frame, font: &Font, client: &bsky_auth::AuthClient) {
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

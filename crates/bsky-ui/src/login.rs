//! Login screen — the first thing the user sees if no session is
//! persisted (or if one is and resume fails).
//!
//! State machine:
//!
//! ```text
//!   CheckingSession  →  Idle           (no session.json or resume failed)
//!                    →  Done(client)   (resume succeeded)
//!
//!   Idle             →  Authenticating (Login pressed with valid fields)
//!                    →  Error          (Login pressed with empty fields)
//!
//!   Authenticating   →  Done(client)   (login_with_password succeeded)
//!                    →  Error          (login_with_password failed)
//!
//!   Done(client)     →  (transition out via ScreenAction::Goto)
//!
//!   Error            →  Idle           (any field tap or button press)
//! ```
//!
//! Blocking work (`try_resume_existing_session`, `login_with_password`)
//! happens in `after_present` so the user sees the "Checking…" /
//! "Authenticating…" frame *before* we freeze the loop.

use std::sync::Arc;

use bsky_auth::{login_with_password, try_resume_existing_session, AuthClient};
use bsky_ime::{Ime, ImeMode, ImeState, TextBoxMode};
use bsky_render::{theme, Color, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};

use crate::profile::ProfileScreen;
use crate::screen::{Screen, ScreenAction};
use crate::widget::{button, text_field, ButtonState, FieldState, Rect, UiCtx};

const FIELD_LEFT: f32 = 200.0;
const FIELD_WIDTH: f32 = 560.0;
const FIELD_HEIGHT: f32 = 40.0;

const HANDLE_RECT: Rect = Rect::new(FIELD_LEFT, 200.0, FIELD_WIDTH, FIELD_HEIGHT);
const PASSWORD_RECT: Rect = Rect::new(FIELD_LEFT, 280.0, FIELD_WIDTH, FIELD_HEIGHT);
const LOGIN_RECT: Rect = Rect::new(FIELD_LEFT, 370.0, FIELD_WIDTH, 50.0);

#[derive(Copy, Clone, PartialEq, Eq)]
enum Focus {
    None,
    Handle,
    Password,
}

enum LoginState {
    /// First frame after construction — try to resume an existing session.
    CheckingSession,
    /// Default form state.
    Idle,
    /// Login pressed; will run auth in `after_present`.
    Authenticating,
    /// Auth succeeded; carries the client to hand off to ProfileScreen.
    Done(AuthClient),
    /// Auth failed; show error message. Tapping a field or the Login
    /// button transitions back to Idle.
    Error(String),
    /// Transient placeholder used by mem::replace when extracting Done.
    /// Should never be observed during a frame.
    _Transitioning,
}

pub struct LoginScreen {
    handle: FieldState,
    password: FieldState,
    login_btn: ButtonState,
    focus: Focus,
    state: LoginState,
}

impl Default for LoginScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginScreen {
    /// Construct in `CheckingSession` state — `after_present` on the
    /// first frame will try to resume; falls back to `Idle` if there's
    /// no on-disk session.
    pub fn new() -> Self {
        Self {
            handle: FieldState::default(),
            password: FieldState::default(),
            login_btn: ButtonState::default(),
            focus: Focus::None,
            state: LoginState::CheckingSession,
        }
    }

    /// Skip the resume check entirely — start directly in Idle.
    /// Used when main.rs already knows there's no session to resume
    /// (e.g. the START-held debug fallback path failed).
    pub fn idle() -> Self {
        let mut s = Self::new();
        s.state = LoginState::Idle;
        s
    }

}

impl Screen for LoginScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        ime: &mut Ime,
    ) -> ScreenAction {
        // ─── 1. Drain any pending IME result into the focused field ────
        match ime.poll() {
            ImeState::Finished(s) => {
                match self.focus {
                    Focus::Handle => self.handle.value = s,
                    Focus::Password => self.password.value = s,
                    Focus::None => {}
                }
                self.handle.focused = false;
                self.password.focused = false;
                self.focus = Focus::None;
                ime.close();
            }
            ImeState::Cancelled | ImeState::Aborted => {
                self.handle.focused = false;
                self.password.focused = false;
                self.focus = Focus::None;
                ime.close();
            }
            _ => {}
        }

        // ─── 2. If we're Done, transition (this is the only place we
        //        consume the AuthClient out of the state). ────────────
        if let LoginState::Done(_) = &self.state {
            // Render a final "Authenticating…" frame so we don't pop the
            // form back briefly while transitioning. Same visual as the
            // Authenticating state.
            self.draw_overlay_message(frame, font, "Authenticating…");
            // Take the client out, wrap in Arc, and emit AuthComplete so
            // main.rs can spawn the worker.
            let prev = core::mem::replace(&mut self.state, LoginState::_Transitioning);
            if let LoginState::Done(client) = prev {
                let client = Arc::new(client);
                let next = Box::new(ProfileScreen::new(Arc::clone(&client), None));
                return ScreenAction::AuthComplete { client, next };
            }
            // Unreachable, but reset just in case.
            self.state = LoginState::Idle;
            return ScreenAction::None;
        }

        // ─── 3. Render the appropriate state. ─────────────────────────
        // Title block (always drawn).
        frame.draw_text_centered(font, 50, theme::TEXT_PRIMARY, 2.0, "bsky-vita");
        frame.draw_text_centered(font, 110, theme::TEXT_MUTED, 1.0, "Sign in to Bluesky");

        let interactive = matches!(self.state, LoginState::Idle | LoginState::Error(_))
            && !ime.is_active();

        // Always lay out the form widgets so visual continuity is preserved
        // across state transitions. Disable click events while non-Idle.
        let h_clicked = text_field(
            frame,
            font,
            HANDLE_RECT,
            "Handle",
            "alice.bsky.social",
            &mut self.handle,
            ctx,
            false,
            interactive,
        );
        let p_clicked = text_field(
            frame,
            font,
            PASSWORD_RECT,
            "App password",
            "app password (xxxx-xxxx-xxxx-xxxx)",
            &mut self.password,
            ctx,
            true,
            interactive,
        );
        let l_clicked = button(
            frame,
            font,
            LOGIN_RECT,
            "Login",
            &mut self.login_btn,
            ctx,
            interactive,
        );

        // State-specific overlays / status lines.
        match &self.state {
            LoginState::CheckingSession => {
                self.draw_overlay_message(frame, font, "Checking saved session…");
            }
            LoginState::Authenticating => {
                self.draw_overlay_message(frame, font, "Authenticating…");
            }
            LoginState::Error(msg) => {
                let line = format!("Error: {msg}");
                frame.draw_text_centered(font, 440, theme::ERROR, 0.95, &line);
            }
            LoginState::Idle => { /* no overlay */ }
            LoginState::Done(_) | LoginState::_Transitioning => { /* handled above */ }
        }

        // Bottom hint line.
        frame.draw_text_centered(
            font,
            SCREEN_HEIGHT - 20,
            theme::TEXT_MUTED,
            0.7,
            "Get an app password at bsky.app/settings/app-passwords",
        );

        // ─── 4. Handle click events (only in Idle/Error). ─────────────
        if h_clicked {
            self.transition_to_idle();
            self.handle.focused = true;
            self.focus = Focus::Handle;
            let _ = ime.open(
                "Handle",
                ImeMode::BasicLatin,
                TextBoxMode::Default,
                64,
                &self.handle.value,
            );
        }
        if p_clicked {
            self.transition_to_idle();
            self.password.focused = true;
            self.focus = Focus::Password;
            let _ = ime.open(
                "App password",
                ImeMode::BasicLatin,
                TextBoxMode::Password,
                64,
                "",
            );
        }
        if l_clicked {
            if self.handle.value.trim().is_empty() || self.password.value.is_empty() {
                self.state = LoginState::Error(
                    "Handle and app password are required".to_string(),
                );
            } else {
                self.state = LoginState::Authenticating;
            }
        }

        ScreenAction::None
    }

    fn after_present(&mut self) {
        match &self.state {
            LoginState::CheckingSession => {
                match try_resume_existing_session() {
                    Ok(Some(client)) => self.state = LoginState::Done(client),
                    Ok(None) => self.state = LoginState::Idle,
                    Err(e) => self.state = LoginState::Error(format!("resume failed: {e}")),
                }
            }
            LoginState::Authenticating => {
                let handle = self.handle.value.trim().to_string();
                let password = self.password.value.clone();
                match login_with_password(&handle, &password) {
                    Ok(client) => self.state = LoginState::Done(client),
                    Err(e) => self.state = LoginState::Error(format!("{e}")),
                }
            }
            _ => {}
        }
    }
}

impl LoginScreen {
    fn transition_to_idle(&mut self) {
        if matches!(self.state, LoginState::Error(_)) {
            self.state = LoginState::Idle;
        }
    }

    fn draw_overlay_message(&self, frame: &mut Frame, font: &Font, msg: &str) {
        // Semi-opaque dim overlay over the form area. We don't have alpha
        // primitives in Phase 2 vita2d wrapper; use a solid dim color
        // covering the form rows.
        let dim = Color::rgba(0x0F, 0x17, 0x2A, 0xC0);
        frame.fill_rect(0.0, 170.0, SCREEN_WIDTH as f32, 270.0, dim);
        frame.draw_text_centered(font, 320, theme::TEXT_PRIMARY, 1.2, msg);
    }
}

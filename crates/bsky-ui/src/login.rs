//! Login screen — the first thing the user sees if no session is
//! persisted. Two text fields (handle, app password) and a Login button.
//!
//! Phase 2.4 is purely visual + interactive: tapping a field opens the
//! IME and stores the typed value in the field's `FieldState.value`.
//! Tapping Login writes a `login pressed: …` line to `last_event` and
//! eprintln (no real auth). Phase 2.5 wires this to `bsky-auth`.

use bsky_ime::{Ime, ImeMode, ImeState, TextBoxMode};
use bsky_render::{theme, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};

use crate::screen::Screen;
use crate::widget::{button, label, text_field, ButtonState, FieldState, Rect, UiCtx};

const FIELD_LEFT: f32 = 200.0;
const FIELD_WIDTH: f32 = 560.0;
const FIELD_HEIGHT: f32 = 40.0;

const HANDLE_RECT: Rect = Rect::new(FIELD_LEFT, 200.0, FIELD_WIDTH, FIELD_HEIGHT);
const PASSWORD_RECT: Rect = Rect::new(FIELD_LEFT, 280.0, FIELD_WIDTH, FIELD_HEIGHT);
const LOGIN_RECT: Rect = Rect::new(FIELD_LEFT, 370.0, FIELD_WIDTH, 50.0);

/// Which field is currently asking the IME for input. Drives where
/// `Finished(s)` from the IME gets routed.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Focus {
    None,
    Handle,
    Password,
}

pub struct LoginScreen {
    handle: FieldState,
    password: FieldState,
    login_btn: ButtonState,
    focus: Focus,
    last_event: String,
}

impl Default for LoginScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginScreen {
    pub fn new() -> Self {
        Self {
            handle: FieldState::default(),
            password: FieldState::default(),
            login_btn: ButtonState::default(),
            focus: Focus::None,
            last_event: String::new(),
        }
    }
}

impl Screen for LoginScreen {
    fn frame(&mut self, frame: &mut Frame, font: &Font, ctx: &UiCtx, ime: &mut Ime) {
        // ─── 1. Drain IME state if a result is pending ──────────────────
        match ime.poll() {
            ImeState::Finished(s) => {
                match self.focus {
                    Focus::Handle => self.handle.value = s,
                    Focus::Password => self.password.value = s,
                    Focus::None => {} // shouldn't happen; ignore
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

        // ─── 2. Draw + hit-test ─────────────────────────────────────────
        // Title block
        frame.draw_text_centered(font, 50, theme::TEXT_PRIMARY, 2.0, "bsky-vita");
        frame.draw_text_centered(font, 110, theme::TEXT_MUTED, 1.0, "Sign in to Bluesky");

        // Disable input while IME is up — clicks shouldn't reach widgets behind it.
        let interactive = !ime.is_active();

        let h_clicked = text_field(
            frame, font, HANDLE_RECT,
            "Handle", "alice.bsky.social",
            &mut self.handle, ctx, false, interactive,
        );

        let p_clicked = text_field(
            frame, font, PASSWORD_RECT,
            "App password", "app password (xxxx-xxxx-xxxx-xxxx)",
            &mut self.password, ctx, true, interactive,
        );

        let l_clicked = button(
            frame, font, LOGIN_RECT,
            "Login",
            &mut self.login_btn, ctx, interactive,
        );

        // Status line at the bottom (slightly above true bottom for breathing room).
        if !self.last_event.is_empty() {
            frame.draw_text_centered(
                font,
                SCREEN_HEIGHT - 50,
                theme::TEXT_MUTED,
                0.9,
                &self.last_event,
            );
        }
        // Hint line — subtle.
        let _ = SCREEN_WIDTH; // silence unused on host
        frame.draw_text_centered(
            font,
            SCREEN_HEIGHT - 20,
            theme::TEXT_MUTED,
            0.7,
            "Get an app password at bsky.app/settings/app-passwords",
        );

        // ─── 3. React to clicks ─────────────────────────────────────────
        if h_clicked {
            self.handle.focused = true;
            self.focus = Focus::Handle;
            // Open with the existing value pre-populated so the user can edit
            // rather than retyping from scratch.
            let _ = ime.open(
                "Handle",
                ImeMode::BasicLatin,
                TextBoxMode::Default,
                64,
                &self.handle.value,
            );
        }
        if p_clicked {
            self.password.focused = true;
            self.focus = Focus::Password;
            let _ = ime.open(
                "App password",
                ImeMode::BasicLatin,
                TextBoxMode::Password,
                64,
                "", // never pre-populate a password
            );
        }
        if l_clicked {
            self.last_event = format!(
                "login pressed: handle={:?}, password_len={} bytes",
                self.handle.value,
                self.password.value.len()
            );
            eprintln!("{}", self.last_event);
            // Phase 2.5 will replace this with bsky_auth::login(...)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rects_dont_overlap() {
        // Sanity: handle / password / login rects should be vertically
        // separated, otherwise touch hit-tests interact.
        assert!(HANDLE_RECT.y + HANDLE_RECT.h < PASSWORD_RECT.y);
        assert!(PASSWORD_RECT.y + PASSWORD_RECT.h < LOGIN_RECT.y);
    }

    #[test]
    fn rects_are_within_screen_width() {
        for r in [HANDLE_RECT, PASSWORD_RECT, LOGIN_RECT] {
            assert!(r.x >= 0.0);
            assert!(r.x + r.w <= SCREEN_WIDTH as f32);
        }
    }
}

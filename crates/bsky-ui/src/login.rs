//! Login screen — the first thing the user sees if no session is
//! persisted (or if one is and resume fails).
//!
//! Phase 11 adds a primary OAuth flow alongside the existing app-password
//! flow. The user lands on the OAuth view by default; "Use an app password
//! instead" is a secondary link that swaps to the legacy form. Both paths
//! produce the same `bsky_auth::AuthClient` (wrapping either
//! `AuthAgent::Password(...)` or `AuthAgent::OAuth(...)`) and exit via the
//! same `ScreenAction::AuthComplete`.
//!
//! State machine (OAuth path):
//!
//! ```text
//!   Idle (Mode::OAuth)
//!     ↓ tap "Sign in with Bluesky" (handle non-empty)
//!   OAuthBeginning   — `after_present` calls `VitaOAuthClient::start_flow`
//!     ↓
//!   OAuthAwaiting    — QR rendered; broker-poll thread runs in background
//!     ↓ poll receives `PollOutcome::Ready { code, iss }`
//!   OAuthExchanging  — `after_present` calls `VitaOAuthClient::complete_flow`
//!     ↓
//!   Done(AuthClient) — emit `AuthComplete`
//! ```
//!
//! Cancel during `OAuthAwaiting` calls `BrokerPoll::cancel`, which signals
//! the polling thread to stop hitting the broker (honored within ~100 ms),
//! then returns to Idle.

use std::sync::Arc;

use bsky_auth::{
    login_with_password, try_resume_existing_session, AuthAgent, AuthClient, ResolvedIdentity,
};
use bsky_ime::{Ime, ImeMode, ImeState, TextBoxMode};
use bsky_input::buttons;
use bsky_oauth::broker::BrokerPoll;
use bsky_oauth::{
    spawn_broker_poll, try_resume_existing_oauth_session, OAuthLoginResult, PendingFlow,
    PollOutcome, Transport, VitaOAuthClient,
};
use bsky_render::{theme, Color, Font, Frame, Texture, SCREEN_HEIGHT, SCREEN_WIDTH};

use crate::profile::ProfileScreen;
use crate::screen::{Screen, ScreenAction};
use crate::widget::{button, text_field, ButtonState, FieldState, Rect, UiCtx};

const FIELD_LEFT: f32 = 200.0;
const FIELD_WIDTH: f32 = 560.0;
const FIELD_HEIGHT: f32 = 40.0;

const HANDLE_RECT: Rect = Rect::new(FIELD_LEFT, 200.0, FIELD_WIDTH, FIELD_HEIGHT);
const PASSWORD_RECT: Rect = Rect::new(FIELD_LEFT, 280.0, FIELD_WIDTH, FIELD_HEIGHT);
const PRIMARY_BTN_RECT: Rect = Rect::new(FIELD_LEFT, 370.0, FIELD_WIDTH, 50.0);
const TOGGLE_LINK_RECT: Rect = Rect::new(FIELD_LEFT, 440.0, FIELD_WIDTH, 28.0);

// OAuth-awaiting layout: QR centered with room above for the title and below
// for a status line + Cancel button. Module size is tuned so a ~250-char URL
// (QR version ~10, ~65 modules including quiet zone) fits at ~325 px square,
// leaving comfortable margins on the 960×544 screen.
const QR_MODULE_PX: u32 = 5;
const QR_TOP_Y: f32 = 100.0;
const CANCEL_BTN_RECT: Rect = Rect::new(380.0, 488.0, 200.0, 40.0);

#[derive(Copy, Clone, PartialEq, Eq)]
enum Focus {
    None,
    Handle,
    Password,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Mode {
    OAuth,
    AppPassword,
}

enum LoginState {
    /// First frame after construction — try to resume an existing OAuth
    /// session, then fall back to app-password, then to Idle.
    CheckingSession,
    /// Default form state.
    Idle,
    /// App-password login pressed; will run auth in `after_present`.
    Authenticating,
    /// Auth succeeded; carries the client to hand off to ProfileScreen.
    Done(AuthClient),
    /// Auth failed; show error message. Tapping a field or button
    /// transitions back to Idle.
    Error(String),
    /// Transient placeholder used by mem::replace when extracting Done.
    _Transitioning,
    /// "Sign in with Bluesky" pressed; `after_present` calls `start_flow`.
    OAuthBeginning,
    /// QR rendered + broker poll thread running. `poll` is drained each
    /// frame via its `rx`; on success/failure/cancel the thread is told to
    /// stop (`poll.cancel()`) before transitioning out of this variant.
    OAuthAwaiting {
        auth_url: String,
        qr: Texture,
        poll: BrokerPoll,
    },
    /// Broker delivered the code; `after_present` calls `complete_flow`.
    OAuthExchanging { code: String, iss: String },
}

pub struct LoginScreen {
    handle: FieldState,
    password: FieldState,
    primary_btn: ButtonState,
    toggle_link_btn: ButtonState,
    cancel_btn: ButtonState,
    focus: Focus,
    mode: Mode,
    state: LoginState,
    /// Constructed at the start of an OAuth flow; held until the flow
    /// completes (`complete_flow` needs the same instance that ran
    /// `start_flow` because the in-memory state store binds them).
    oauth_client: Option<VitaOAuthClient>,
    pending_flow: Option<PendingFlow>,
    /// `Cancel` was clicked (or CIRCLE pressed) during `OAuthAwaiting`.
    /// Deferred to `after_present` so the GPU has finished THIS frame's
    /// draws (including the QR texture) before its `Drop` impl frees the
    /// vita2d memory — otherwise the present-time draw of the QR
    /// references freed memory → GPUCRASH.
    pending_cancel: bool,
    /// Broker poll returned a code while in `OAuthAwaiting`. Same
    /// deferred-drop reason as `pending_cancel` (the transition to
    /// `OAuthExchanging` drops the QR texture).
    pending_oauth_ready: Option<(String, String)>,
    /// Poll thread returned Failed/Timeout. Same deferred-drop reason.
    pending_oauth_failed: Option<String>,
}

impl Default for LoginScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginScreen {
    /// Construct in `CheckingSession` state — `after_present` on the
    /// first frame tries OAuth resume, then app-password resume.
    pub fn new() -> Self {
        Self {
            handle: FieldState::default(),
            password: FieldState::default(),
            primary_btn: ButtonState::default(),
            toggle_link_btn: ButtonState::default(),
            cancel_btn: ButtonState::default(),
            focus: Focus::None,
            mode: Mode::OAuth,
            state: LoginState::CheckingSession,
            oauth_client: None,
            pending_flow: None,
            pending_cancel: false,
            pending_oauth_ready: None,
            pending_oauth_failed: None,
        }
    }

    /// Skip the resume check entirely — start directly in Idle. Used
    /// post-logout to avoid bouncing back into the just-cleared session.
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
        // ─── 1. Drain pending IME result. ──────────────────────────────
        match ime.poll() {
            ImeState::Finished(s) => {
                match self.focus {
                    // Handles are case-insensitive domain names per atproto;
                    // canonical form is lowercase. Normalize on input so the
                    // displayed field matches what gets sent (and so users
                    // who type a capital don't get "handle not found").
                    Focus::Handle => self.handle.value = s.trim().to_lowercase(),
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

        // ─── 2. Drain the broker poll receiver each frame while
        //        OAuthAwaiting. Records the outcome into a `pending_*`
        //        field; the actual state transition (which drops the QR
        //        Texture) is deferred to `after_present` so the GPU has
        //        finished using the texture for THIS frame's draws.
        //        Otherwise: GPU references freed memory → GPUCRASH. ─────
        if let LoginState::OAuthAwaiting { poll, .. } = &self.state {
            match poll.rx.try_recv() {
                Ok(PollOutcome::Ready { code, iss }) => {
                    bsky_log::log!("oauth: login got Ready code_len={} iss={}", code.len(), iss);
                    self.pending_oauth_ready = Some((code, iss));
                }
                Ok(PollOutcome::Timeout) => {
                    bsky_log::log!("oauth: login got Timeout");
                    self.pending_oauth_failed =
                        Some("Sign-in timed out — please try again.".into());
                }
                Ok(PollOutcome::Failed(m)) => {
                    bsky_log::log!("oauth: login got Failed: {m}");
                    self.pending_oauth_failed = Some(format!("Broker: {m}"));
                }
                Err(_) => {}
            }
        }

        // ─── 3. If we're Done, transition. ────────────────────────────
        if let LoginState::Done(_) = &self.state {
            self.draw_overlay_message(frame, font, "Authenticating…");
            let prev = core::mem::replace(&mut self.state, LoginState::_Transitioning);
            if let LoginState::Done(client) = prev {
                let client = Arc::new(client);
                let next = Box::new(ProfileScreen::new(Arc::clone(&client), None));
                return ScreenAction::AuthComplete { client, next };
            }
            self.state = LoginState::Idle;
            return ScreenAction::None;
        }

        // ─── 4. Render. Branch on whether we're in the OAuth-awaiting
        //        sub-screen (QR view) or the normal form layout. ──────
        match &self.state {
            LoginState::OAuthAwaiting { auth_url, qr, poll } => {
                self.render_oauth_awaiting(frame, font, qr, auth_url);
                let cancel_clicked =
                    button(frame, font, CANCEL_BTN_RECT, "Cancel", &mut self.cancel_btn, ctx, true);
                let pad_cancel = ctx.pad.just_pressed(buttons::CIRCLE);
                if cancel_clicked || pad_cancel {
                    // Tell the poll thread to stop hitting the broker right
                    // away (honored within ~100 ms). The actual state mutation
                    // (which drops the QR Texture *and* the BrokerPoll) is
                    // deferred to `after_present` — see `pending_cancel` doc;
                    // wait_rendering_done() runs there before the drop. The
                    // BrokerPoll's own Drop also cancels + joins, so this
                    // explicit call is just to stop polling promptly.
                    poll.cancel();
                    self.pending_cancel = true;
                }
                return ScreenAction::None;
            }
            LoginState::OAuthExchanging { .. } => {
                self.draw_overlay_message(frame, font, "Completing sign-in…");
                return ScreenAction::None;
            }
            _ => {}
        }

        // Normal form layout (Idle / Authenticating / Error / CheckingSession / OAuthBeginning).
        self.render_form_header(frame, font);

        let interactive = matches!(self.state, LoginState::Idle | LoginState::Error(_))
            && !ime.is_active();

        // Handle field is shared between both modes.
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

        let (p_clicked, primary_label, primary_clicked, toggle_label) = match self.mode {
            Mode::AppPassword => {
                let p = text_field(
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
                let l = button(
                    frame,
                    font,
                    PRIMARY_BTN_RECT,
                    "Log in",
                    &mut self.primary_btn,
                    ctx,
                    interactive,
                );
                (p, "Log in", l, "Sign in with Bluesky (recommended)")
            }
            Mode::OAuth => {
                let l = button(
                    frame,
                    font,
                    PRIMARY_BTN_RECT,
                    "Sign in with Bluesky",
                    &mut self.primary_btn,
                    ctx,
                    interactive,
                );
                (false, "Sign in with Bluesky", l, "Use an app password instead")
            }
        };

        // Toggle-mode link (small underlined-style accent text). Lower than
        // the primary button; functions as a button.
        let t_clicked = button(
            frame,
            font,
            TOGGLE_LINK_RECT,
            toggle_label,
            &mut self.toggle_link_btn,
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
            LoginState::OAuthBeginning => {
                self.draw_overlay_message(frame, font, "Starting OAuth flow…");
            }
            LoginState::Error(msg) => {
                // y=485 sits in the gap between the toggle-link button (ends
                // around y=468) and the bottom hint pair (starts at y=508),
                // so the error stays readable instead of being covered by
                // the explainer text.
                let line = format!("Error: {msg}");
                frame.draw_text_centered(font, 485, theme::ERROR, 0.9, &line);
            }
            _ => {}
        }

        // Bottom hints. The DM-scope tip only applies to the app-password
        // path (OAuth grants chat.bsky scope via consent screen).
        let hint = match self.mode {
            Mode::OAuth => "OAuth signs you in via your phone — no password typing on the Vita.",
            Mode::AppPassword => {
                "Tip: enable \"Allow access to your direct messages\" on the app password for DMs"
            }
        };
        frame.draw_text_centered(font, SCREEN_HEIGHT - 36, theme::TEXT_MUTED, 0.7, hint);
        frame.draw_text_centered(
            font,
            SCREEN_HEIGHT - 18,
            theme::TEXT_MUTED,
            0.7,
            "Get an app password at bsky.app/settings/app-passwords",
        );

        // ─── 5. Handle clicks. ─────────────────────────────────────────
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
        if primary_clicked {
            self.handle_primary(primary_label);
        }
        if t_clicked {
            self.mode = match self.mode {
                Mode::OAuth => Mode::AppPassword,
                Mode::AppPassword => Mode::OAuth,
            };
            self.transition_to_idle();
        }

        ScreenAction::None
    }

    fn after_present(&mut self) {
        // Handle deferred transitions out of `OAuthAwaiting` here so the
        // QR Texture's Drop happens AFTER the present has completed and
        // the GPU has finished using it. `wait_rendering_done()` blocks
        // until the GPU is idle; the subsequent state replacement drops
        // the previous `OAuthAwaiting` variant (including its Texture),
        // which now safely calls `vita2d_free_texture`.
        if self.pending_cancel
            || self.pending_oauth_ready.is_some()
            || self.pending_oauth_failed.is_some()
        {
            bsky_render::wait_rendering_done();
            if self.pending_cancel {
                self.pending_cancel = false;
                self.oauth_client = None;
                self.pending_flow = None;
                self.state = LoginState::Idle;
                return;
            }
            if let Some((code, iss)) = self.pending_oauth_ready.take() {
                // Exchange flow keeps oauth_client + pending_flow; just
                // moves out of the QR-holding variant.
                self.state = LoginState::OAuthExchanging { code, iss };
                // Fall through so we also run the OAuthExchanging arm
                // below in the same after_present tick — saves one frame
                // of "Completing sign-in…" overlay before the network call
                // starts.
            } else if let Some(msg) = self.pending_oauth_failed.take() {
                self.oauth_client = None;
                self.pending_flow = None;
                self.state = LoginState::Error(msg);
                return;
            }
        }

        match &self.state {
            LoginState::CheckingSession => {
                // OAuth resume first, then password.
                match try_resume_existing_oauth_session() {
                    Ok(Some(result)) => {
                        self.state = LoginState::Done(into_auth_client_oauth(result));
                        return;
                    }
                    Ok(None) => { /* fall through */ }
                    Err(e) => {
                        bsky_log::log!("oauth resume bounced: {e}");
                        // fall through to password
                    }
                }
                match try_resume_existing_session() {
                    Ok(Some(client)) => self.state = LoginState::Done(client),
                    Ok(None) => self.state = LoginState::Idle,
                    Err(e) => {
                        self.state = LoginState::Error(format!("resume failed: {e}"))
                    }
                }
            }
            LoginState::Authenticating => {
                let handle = self.handle.value.trim().to_lowercase();
                let password = self.password.value.clone();
                match login_with_password(&handle, &password) {
                    Ok(client) => self.state = LoginState::Done(client),
                    Err(e) => self.state = LoginState::Error(format!("{e}")),
                }
            }
            LoginState::OAuthBeginning => {
                let handle = self.handle.value.trim().to_lowercase();
                let client = match VitaOAuthClient::new() {
                    Ok(c) => c,
                    Err(e) => {
                        self.state = LoginState::Error(format!("OAuth client init: {e}"));
                        return;
                    }
                };
                match client.start_flow(&handle, Transport::Broker) {
                    Ok((url, pending)) => {
                        let qr = match Texture::from_qr_string(&url, QR_MODULE_PX) {
                            Ok(t) => t,
                            Err(e) => {
                                self.state =
                                    LoginState::Error(format!("QR encode: {e}"));
                                return;
                            }
                        };
                        let poll = spawn_broker_poll(pending.state.clone());
                        self.oauth_client = Some(client);
                        self.pending_flow = Some(pending);
                        self.state = LoginState::OAuthAwaiting {
                            auth_url: url,
                            qr,
                            poll,
                        };
                    }
                    Err(e) => {
                        self.state = LoginState::Error(format!("OAuth start: {e}"));
                    }
                }
            }
            LoginState::OAuthExchanging { code, iss } => {
                let code = code.clone();
                let iss = iss.clone();
                let Some(client) = self.oauth_client.as_ref() else {
                    self.state = LoginState::Error("internal: missing OAuth client".into());
                    return;
                };
                let Some(pending) = self.pending_flow.take() else {
                    self.state = LoginState::Error("internal: missing pending flow".into());
                    return;
                };
                match client.complete_flow(pending, &code, &iss) {
                    Ok(result) => {
                        self.oauth_client = None;
                        self.state = LoginState::Done(into_auth_client_oauth(result));
                    }
                    Err(e) => {
                        self.oauth_client = None;
                        self.state = LoginState::Error(format!("OAuth exchange: {e}"));
                    }
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

    fn handle_primary(&mut self, _label: &str) {
        match self.mode {
            Mode::AppPassword => {
                if self.handle.value.trim().is_empty() || self.password.value.is_empty() {
                    self.state =
                        LoginState::Error("Handle and app password are required".into());
                } else {
                    self.state = LoginState::Authenticating;
                }
            }
            Mode::OAuth => {
                if self.handle.value.trim().is_empty() {
                    self.state = LoginState::Error("Handle is required".into());
                } else {
                    self.state = LoginState::OAuthBeginning;
                }
            }
        }
    }

    fn render_form_header(&self, frame: &mut Frame, font: &Font) {
        frame.draw_text_centered(font, 50, theme::TEXT_PRIMARY, 2.0, "bsky-vita");
        let sub = match self.mode {
            Mode::OAuth => "Sign in with Bluesky",
            Mode::AppPassword => "Sign in with an app password",
        };
        frame.draw_text_centered(font, 110, theme::TEXT_MUTED, 1.0, sub);
    }

    fn render_oauth_awaiting(
        &self,
        frame: &mut Frame,
        font: &Font,
        qr: &Texture,
        auth_url: &str,
    ) {
        frame.draw_text_centered(font, 28, theme::TEXT_PRIMARY, 1.4, "Sign in on your phone");
        frame.draw_text_centered(
            font,
            70,
            theme::TEXT_MUTED,
            0.8,
            "Open your phone's camera and scan the code below.",
        );
        // Center the QR horizontally.
        let qr_w = qr.width() as f32;
        let x = (SCREEN_WIDTH as f32 - qr_w) / 2.0;
        frame.draw_texture(qr, x, QR_TOP_Y);
        // Waiting indicator below the QR (the broker poll runs every 2 s with
        // a 5-min deadline; CF KV propagation can add a few seconds after the
        // user consents on their phone).
        frame.draw_text_centered(
            font,
            QR_TOP_Y as i32 + qr.height() + 14,
            theme::TEXT_MUTED,
            0.75,
            "Waiting for sign-in to complete…",
        );
        let _ = auth_url; // intentionally not displayed (too long for a hint line)
    }

    fn draw_overlay_message(&self, frame: &mut Frame, font: &Font, msg: &str) {
        let dim = Color::rgba(0x0F, 0x17, 0x2A, 0xC0);
        frame.fill_rect(0.0, 170.0, SCREEN_WIDTH as f32, 270.0, dim);
        frame.draw_text_centered(font, 320, theme::TEXT_PRIMARY, 1.2, msg);
    }
}

/// Bridge from `bsky_oauth::OAuthLoginResult` to `bsky_auth::AuthClient`
/// — wraps the OAuth agent into the `AuthAgent::OAuth` variant.
fn into_auth_client_oauth(result: OAuthLoginResult) -> AuthClient {
    let OAuthLoginResult { agent, resolved } = result;
    // The atrium-identity ResolvedIdentity shape matches bsky-auth's
    // re-export. Re-bind to be explicit.
    let resolved: ResolvedIdentity = resolved;
    AuthClient {
        agent: AuthAgent::OAuth(agent),
        resolved,
    }
}

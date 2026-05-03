//! Compose screen — full-screen modal for writing a new post or reply.
//!
//! Pushed onto the navigation stack from TimelineScreen (SQUARE button
//! or the floating "+" → top-level compose; R on focused post → reply).
//! The screen takes over input until the user submits or cancels:
//!
//! - Tap the text area → open IME with current text (300-grapheme cap).
//! - Tap **Cancel** → `Pop` (back to timeline; current draft discarded).
//! - Tap **Post** when text is non-empty → dispatch `CreatePost` to the
//!   worker; on `WorkResponse::PostCreated(Ok)` → `Pop`.
//! - On `Err`, render the error inline and stay editing.
//!
//! Phase 4.2 is text-only — no embeds, no facets (mention/url/tag
//! linkification), no character-by-character IME (the SDK's IME is
//! modal). Phase 4.x can refine.

use std::sync::Arc;

use bsky_auth::AuthClient;
use bsky_ime::{Ime, ImeMode, ImeState, TextBoxMode};
use bsky_input::buttons;
use bsky_render::{theme, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{ReplyTarget, WorkRequest, WorkResponse};

use crate::screen::{Screen, ScreenAction};
use crate::widget::{button, ButtonState, Rect, UiCtx};

/// Bluesky's grapheme cap for posts. Phase 4.2 approximates with
/// `chars().count()` — adequate for ASCII + most Latin/CJK; users
/// composing emoji-heavy posts may see slight over-/under-counts.
const POST_LIMIT: usize = 300;

#[derive(Default)]
enum ComposeState {
    /// Default — user is editing.
    #[default]
    Editing,
    /// CreatePost dispatched, waiting on worker.
    Submitting,
    /// CreatePost returned an error; stay on screen, allow re-edit.
    Error(String),
}

pub struct ComposeScreen {
    /// Held to construct future requests (image upload, etc. in 4.x).
    /// Worker has its own clone for the actual createRecord call.
    #[allow(dead_code)]
    client: Arc<AuthClient>,
    /// `None` ⇒ top-level post; `Some` ⇒ reply with this target.
    reply_to: Option<ReplyTarget>,
    /// `Some` carries the @handle of the post being replied to (for the
    /// "Replying to @handle" header line). Always `Some` when `reply_to`
    /// is `Some`.
    reply_handle: Option<String>,
    /// Current text buffer.
    text: String,
    state: ComposeState,
    cancel_btn: ButtonState,
    post_btn: ButtonState,
    text_area_btn: ButtonState,
    /// Set to `true` by `handle_worker_response` on a successful post.
    /// The next `frame()` returns `Pop`.
    done: bool,
}

impl ComposeScreen {
    pub fn new(
        client: Arc<AuthClient>,
        reply_to: Option<ReplyTarget>,
        reply_handle: Option<String>,
    ) -> Self {
        Self {
            client,
            reply_to,
            reply_handle,
            text: String::new(),
            state: ComposeState::default(),
            cancel_btn: ButtonState::default(),
            post_btn: ButtonState::default(),
            text_area_btn: ButtonState::default(),
            done: false,
        }
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    fn can_post(&self) -> bool {
        let n = self.char_count();
        matches!(self.state, ComposeState::Editing | ComposeState::Error(_))
            && n > 0
            && n <= POST_LIMIT
    }
}

impl Screen for ComposeScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        ime: &mut Ime,
    ) -> ScreenAction {
        // ─── 0. If a worker response just landed and post succeeded,
        //        pop the screen on this frame. ───────────────────────
        if self.done {
            return ScreenAction::Pop;
        }

        // ─── 1. Drain pending IME result into the text buffer. ─────────
        match ime.poll() {
            ImeState::Finished(s) => {
                self.text = s;
                ime.close();
            }
            ImeState::Cancelled | ImeState::Aborted => {
                ime.close();
            }
            _ => {}
        }

        // ─── 2. CIRCLE → Pop (Cancel). ─────────────────────────────────
        if !ime.is_active() && ctx.pad.just_pressed(buttons::CIRCLE) {
            return ScreenAction::Pop;
        }

        // ─── 3. Title bar: Cancel  |  "New post" / "Replying"  |  Post.
        let header_h = 56;
        frame.fill_rect(
            0.0,
            0.0,
            SCREEN_WIDTH as f32,
            header_h as f32,
            theme::FIELD_BG,
        );
        frame.fill_rect(
            0.0,
            header_h as f32 - 1.0,
            SCREEN_WIDTH as f32,
            1.0,
            theme::TEXT_MUTED,
        );

        let interactive = !ime.is_active() && !matches!(self.state, ComposeState::Submitting);

        // Cancel (left).
        let cancel_clicked = button(
            frame,
            font,
            Rect::new(8.0, 8.0, 100.0, 40.0),
            "Cancel",
            &mut self.cancel_btn,
            ctx,
            interactive,
        );
        if cancel_clicked {
            return ScreenAction::Pop;
        }

        // Title (center).
        let title = if self.reply_to.is_some() {
            "Reply"
        } else {
            "New post"
        };
        frame.draw_text_centered(font, 36, theme::TEXT_PRIMARY, 1.1, title);

        // Post (right).
        let post_rect = Rect::new(SCREEN_WIDTH as f32 - 108.0, 8.0, 100.0, 40.0);
        let can_post = self.can_post();
        let post_clicked = button(
            frame,
            font,
            post_rect,
            "Post",
            &mut self.post_btn,
            ctx,
            can_post,
        );
        if post_clicked {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::CreatePost {
                    text: self.text.clone(),
                    reply_to: self.reply_to.clone(),
                });
                self.state = ComposeState::Submitting;
            }
        }

        // ─── 4. Reply context line (if any). ───────────────────────────
        let mut content_top = header_h + 12;
        if let Some(handle) = self.reply_handle.as_deref() {
            let line = format!("Replying to @{handle}");
            frame.draw_text(
                font,
                16,
                content_top + 16,
                theme::TEXT_MUTED,
                0.9,
                &line,
            );
            content_top += 28;
        }

        // ─── 5. Text area (tappable). ──────────────────────────────────
        let counter_h = 32;
        let area_rect = Rect::new(
            12.0,
            content_top as f32,
            SCREEN_WIDTH as f32 - 24.0,
            (SCREEN_HEIGHT - content_top - counter_h - 12) as f32,
        );
        // Faint background to indicate the tappable region.
        frame.fill_rect(
            area_rect.x,
            area_rect.y,
            area_rect.w,
            area_rect.h,
            theme::FIELD_BG,
        );
        // Text content (or placeholder).
        let body = if self.text.is_empty() {
            if self.reply_to.is_some() {
                "Tap to write your reply…"
            } else {
                "Tap to write your post…"
            }
        } else {
            self.text.as_str()
        };
        let body_color = if self.text.is_empty() {
            theme::TEXT_MUTED
        } else {
            theme::TEXT_PRIMARY
        };
        frame.draw_text_wrapped_with_emoji(
            font,
            (area_rect.x as i32) + 12,
            (area_rect.y as i32) + 24,
            (area_rect.w as i32) - 24,
            body_color,
            1.0,
            body,
            ctx.emoji,
        );
        let area_clicked = button_invisible(
            frame,
            area_rect,
            &mut self.text_area_btn,
            ctx,
            interactive,
        );
        if area_clicked {
            let _ = ime.open(
                if self.reply_to.is_some() { "Reply" } else { "New post" },
                ImeMode::Default,
                TextBoxMode::Default,
                POST_LIMIT as u32,
                &self.text,
            );
        }

        // ─── 6. Char counter + Submitting/Error overlays. ──────────────
        let n = self.char_count();
        let count_str = format!("{n} / {POST_LIMIT}");
        let count_color = if n > POST_LIMIT {
            theme::ERROR
        } else {
            theme::TEXT_MUTED
        };
        frame.draw_text(
            font,
            16,
            SCREEN_HEIGHT - 12,
            count_color,
            0.85,
            &count_str,
        );
        if let ComposeState::Submitting = &self.state {
            frame.draw_text(
                font,
                SCREEN_WIDTH / 2 - 60,
                SCREEN_HEIGHT - 12,
                theme::TEXT_MUTED,
                0.85,
                "Posting…",
            );
        }
        if let ComposeState::Error(msg) = &self.state {
            let truncated: String = msg.chars().take(80).collect();
            let line = format!("Error: {truncated}");
            frame.draw_text(
                font,
                SCREEN_WIDTH / 2 - 200,
                SCREEN_HEIGHT - 12,
                theme::ERROR,
                0.85,
                &line,
            );
        }

        ScreenAction::None
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        if let WorkResponse::PostCreated(result) = resp {
            match result {
                Ok(_uri) => {
                    // Successful post → main.rs sees the screen want to
                    // pop on the next frame. Set a flag via a state…
                    // actually simpler: encode "post succeeded → pop"
                    // by exposing a way to emit ScreenAction::Pop on
                    // the next frame. We don't have that mechanism, so
                    // instead the ComposeScreen pops itself by storing
                    // a `done: bool` and returning Pop at the start of
                    // the next frame.
                    self.state = ComposeState::Editing;
                    self.text.clear();
                    self.done = true;
                }
                Err(msg) => {
                    self.state = ComposeState::Error(msg);
                }
            }
        }
    }
}

/// Invisible-but-tappable region. Doesn't render any visuals (the
/// caller already filled the rect); just runs the press-tracking
/// state-machine and returns `true` on a clean click.
fn button_invisible(
    _frame: &mut Frame,
    rect: Rect,
    state: &mut ButtonState,
    ctx: &UiCtx,
    enabled: bool,
) -> bool {
    let pressed_now = ctx.touches.iter().any(|t| rect.contains(t.x, t.y));
    let clicked = enabled && state.pressed_last && !pressed_now && ctx.touches.is_empty();
    state.pressed_last = pressed_now;
    clicked
}

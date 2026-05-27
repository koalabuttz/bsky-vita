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
use bsky_media::jpeg;
use bsky_render::{theme, Font, Frame, Texture, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{ComposedImage, ReplyTarget, WorkRequest, WorkResponse};

use crate::file_picker::{FilePicker, PickResult};
use crate::screen::{Screen, ScreenAction};
use crate::widget::{button, ButtonState, Rect, UiCtx};

/// Bluesky's grapheme cap for posts. Phase 4.2 approximates with
/// `chars().count()` — adequate for ASCII + most Latin/CJK; users
/// composing emoji-heavy posts may see slight over-/under-counts.
const POST_LIMIT: usize = 300;

/// Max size of the attached-image preview texture in the compose strip.
const PREVIEW_W: u32 = 152;
const PREVIEW_H: u32 = 100;

/// Upload byte cap (bsky.social rejects larger image blobs). Images over
/// this are downscaled + re-encoded to JPEG before upload.
const MAX_UPLOAD_BYTES: usize = 1_000_000;
/// Longest-edge target when downscaling an oversized image.
const DOWNSCALE_EDGE: u32 = 1600;

/// Height of the close bar in the full-screen image view; the image is
/// fit + centered in the area below it so the bar never covers it.
const FULLVIEW_BAR_H: i32 = 30;

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
    add_image_btn: ButtonState,
    remove_img_btn: ButtonState,
    /// Modal file picker; `Some` while the user is choosing an image.
    picker: Option<FilePicker>,
    /// Path of the chosen image (the upload source; step 5 reads + uploads
    /// these bytes).
    attached_path: Option<String>,
    /// Decoded preview of the attached image (downscaled). `None` while
    /// loading or if decode failed.
    preview: Option<Texture>,
    /// Upload-ready bytes of the attached image (downscaled + re-encoded
    /// if the source was oversized). Kept for the full-screen preview and
    /// the upload. `None` when nothing is attached.
    attached_bytes: Option<Vec<u8>>,
    /// MIME type for `attached_bytes` (image/jpeg or image/png).
    attached_mime: String,
    /// Full-screen image-preview modal: tapping the thumbnail opens it.
    viewing_full: bool,
    full_tex: Option<Texture>,
    preview_btn: ButtonState,
    full_close_btn: ButtonState,
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
            add_image_btn: ButtonState::default(),
            remove_img_btn: ButtonState::default(),
            picker: None,
            attached_path: None,
            preview: None,
            attached_bytes: None,
            attached_mime: String::new(),
            viewing_full: false,
            full_tex: None,
            preview_btn: ButtonState::default(),
            full_close_btn: ButtonState::default(),
            done: false,
        }
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    fn can_post(&self) -> bool {
        let n = self.char_count();
        if n > POST_LIMIT
            || !matches!(self.state, ComposeState::Editing | ComposeState::Error(_))
        {
            return false;
        }
        // If an image is attached, hold Post until its bytes have loaded
        // (otherwise we'd post without the intended image).
        if self.attached_path.is_some() && self.attached_bytes.is_none() {
            return false;
        }
        // Post needs text OR an image.
        n > 0 || self.attached_path.is_some()
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

        // ─── 0b. Modal file picker takes over the whole frame. ─────────
        if self.picker.is_some() {
            match self.picker.as_mut().unwrap().render(frame, font, ctx) {
                Some(PickResult::Picked(path)) => {
                    // Request the full bytes to build a preview (and, in
                    // step 5, to upload). Keyed by path → arrives via
                    // handle_worker_response once the picker is closed.
                    self.preview = None;
                    self.attached_bytes = None;
                    self.attached_mime.clear();
                    self.full_tex = None; // discard prior image's full view
                    if let Some(worker) = ctx.worker {
                        worker.send(WorkRequest::ReadImageFile { path: path.clone() });
                    }
                    self.attached_path = Some(path);
                    self.picker = None;
                }
                Some(PickResult::Cancelled) => self.picker = None,
                None => {}
            }
            return ScreenAction::None;
        }

        // ─── 0c. Full-screen image preview modal. ──────────────────────
        if self.viewing_full {
            frame.fill_rect(
                0.0,
                0.0,
                SCREEN_WIDTH as f32,
                SCREEN_HEIGHT as f32,
                bsky_render::Color::rgb(0x00, 0x00, 0x00),
            );
            if let Some(tex) = &self.full_tex {
                // Center in the area BELOW the close bar so the bar never
                // covers the image.
                let dx = (SCREEN_WIDTH - tex.width()) / 2;
                let dy = FULLVIEW_BAR_H + (SCREEN_HEIGHT - FULLVIEW_BAR_H - tex.height()) / 2;
                frame.draw_texture(tex, dx as f32, dy as f32);
            }
            frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, FULLVIEW_BAR_H as f32, theme::FIELD_BG);
            frame.draw_text(
                font,
                12,
                21,
                theme::TEXT_PRIMARY,
                0.9,
                "Tap, or press O / X, to close",
            );
            let pressed_now = !ctx.touches.is_empty();
            let tapped =
                self.full_close_btn.pressed_last && !pressed_now && ctx.touches.is_empty();
            self.full_close_btn.pressed_last = pressed_now;
            if tapped
                || ctx.pad.just_pressed(buttons::CIRCLE)
                || ctx.pad.just_pressed(buttons::CROSS)
            {
                // Keep `full_tex` alive — freeing it now (same frame it was
                // just drawn) makes the GPU read freed memory → GPUCRASH.
                // It's freed later on Remove/Change, in a frame where it
                // isn't drawn. Reused if the user reopens the full view.
                self.viewing_full = false;
                self.full_close_btn.pressed_last = false;
            }
            return ScreenAction::None;
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
                // Build the image attachment from the loaded bytes (mime by
                // extension). can_post() guarantees bytes are present if a
                // path is attached.
                let images = match &self.attached_bytes {
                    Some(bytes) => vec![ComposedImage {
                        bytes: bytes.clone(),
                        mime: self.attached_mime.clone(),
                        alt: String::new(),
                    }],
                    None => Vec::new(),
                };
                worker.send(WorkRequest::CreatePost {
                    text: self.text.clone(),
                    reply_to: self.reply_to.clone(),
                    images,
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
        // Reserve a strip below the text area for the image attachment
        // controls (Phase 7).
        let counter_h = 32;
        // Tall enough for the preview only when something is attached; a
        // single Add-image button row otherwise (no dead space).
        let attach_h = if self.attached_path.is_some() { 112 } else { 44 };
        let area_h = SCREEN_HEIGHT - content_top - counter_h - 12 - attach_h - 8;
        let area_rect = Rect::new(
            12.0,
            content_top as f32,
            SCREEN_WIDTH as f32 - 24.0,
            area_h as f32,
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

        // ─── 5b. Attachment area: Add/Change + preview + Remove. ───────
        let attach_y = content_top + area_h + 8;
        let add_label = if self.attached_path.is_some() {
            "Change image"
        } else {
            "Add image"
        };
        if button(
            frame,
            font,
            Rect::new(12.0, attach_y as f32, 150.0, 40.0),
            add_label,
            &mut self.add_image_btn,
            ctx,
            interactive,
        ) {
            self.picker = Some(FilePicker::new());
        }
        // Owned clone so we don't hold a borrow on self across the Remove
        // mutation below.
        if let Some(path) = self.attached_path.clone() {
            if button(
                frame,
                font,
                Rect::new(12.0, (attach_y + 48) as f32, 150.0, 40.0),
                "Remove",
                &mut self.remove_img_btn,
                ctx,
                interactive,
            ) {
                self.attached_path = None;
                self.preview = None;
                self.attached_bytes = None;
                self.attached_mime.clear();
                self.full_tex = None; // safe: not in full-view this frame
            } else {
                let px = 176;
                let has_preview = self.preview.is_some();
                if let Some(tex) = &self.preview {
                    frame.draw_texture(tex, px as f32, attach_y as f32);
                } else {
                    frame.draw_text(font, px, attach_y + 24, theme::TEXT_MUTED, 0.9, "loading preview…");
                }
                // Tap the preview → full-screen view.
                let preview_rect =
                    Rect::new(px as f32, attach_y as f32, PREVIEW_W as f32, PREVIEW_H as f32);
                if has_preview
                    && button_invisible(frame, preview_rect, &mut self.preview_btn, ctx, interactive)
                {
                    // Decode the full-size texture once; reuse it on reopen.
                    if self.full_tex.is_none() {
                        if let Some(b) = &self.attached_bytes {
                            self.full_tex = Texture::decode_scaled(
                                b,
                                SCREEN_WIDTH as u32,
                                (SCREEN_HEIGHT - FULLVIEW_BAR_H) as u32,
                            )
                            .ok();
                        }
                    }
                    self.viewing_full = true;
                }
                let name = path.rsplit('/').next().unwrap_or(path.as_str());
                let shown: String = name.chars().take(40).collect();
                frame.draw_text(
                    font,
                    px + PREVIEW_W as i32 + 14,
                    attach_y + 24,
                    theme::TEXT_PRIMARY,
                    0.9,
                    &shown,
                );
                frame.draw_text(
                    font,
                    px + PREVIEW_W as i32 + 14,
                    attach_y + 50,
                    theme::TEXT_MUTED,
                    0.8,
                    "tap to view full size",
                );
            }
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
        // Route local-file image reads (picker thumbnails) into the picker.
        if let WorkResponse::Image { url, bytes } = &resp {
            if let Some(picker) = self.picker.as_mut() {
                picker.on_image(url, bytes);
            } else if self.attached_path.as_deref() == Some(url.as_str()) {
                // Bytes for the attached image → build the compose preview.
                if let Ok(b) = bytes {
                    // Downscale + re-encode if oversized, so the upload
                    // stays under the blob cap.
                    let (upload_bytes, mime) = fit_for_upload(b, url);
                    self.preview = Texture::decode_scaled(&upload_bytes, PREVIEW_W, PREVIEW_H).ok();
                    self.attached_bytes = Some(upload_bytes);
                    self.attached_mime = mime;
                }
            }
            return;
        }
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

/// MIME type from a file path's extension (defaults to JPEG).
fn mime_from_path(path: &str) -> String {
    if path.to_ascii_lowercase().ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    }
    .to_string()
}

/// Produce upload-ready bytes + MIME for an attached image. In-spec
/// images upload as-is; oversized ones are decoded, downscaled to
/// `DOWNSCALE_EDGE`, and re-encoded to JPEG (lowering quality if still
/// over the cap). On any decode/encode failure, falls back to the raw
/// bytes (the server will reject if truly too large).
fn fit_for_upload(raw: &[u8], path: &str) -> (Vec<u8>, String) {
    if raw.len() <= MAX_UPLOAD_BYTES {
        return (raw.to_vec(), mime_from_path(path));
    }
    let Ok((rgba, w, h)) = Texture::decode_scaled_rgba(raw, DOWNSCALE_EDGE, DOWNSCALE_EDGE) else {
        return (raw.to_vec(), mime_from_path(path));
    };
    for quality in [85u8, 70, 55] {
        if let Ok(jpeg) = jpeg::encode_rgba(&rgba, w, h, quality) {
            if jpeg.len() <= MAX_UPLOAD_BYTES || quality == 55 {
                return (jpeg, "image/jpeg".to_string());
            }
        }
    }
    (raw.to_vec(), mime_from_path(path))
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

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
use bsky_worker::{ComposedImage, ReplyTarget, ThreadSegment, WorkRequest, WorkResponse};

use crate::camera::{CameraCapture, CameraResult};
use crate::file_picker::{FilePicker, PickResult};
use crate::imgutil::decode_thumb;
use crate::screen::{Screen, ScreenAction};
use crate::widget::{button, ButtonState, Rect, UiCtx};

/// Bluesky's grapheme cap for posts. Phase 4.2 approximates with
/// `chars().count()` — adequate for ASCII + most Latin/CJK; users
/// composing emoji-heavy posts may see slight over-/under-counts.
const POST_LIMIT: usize = 300;

/// Bluesky's max images per post.
const MAX_IMAGES: usize = 4;
/// Alt-text length cap for the IME.
const ALT_LIMIT: usize = 1000;

/// Attachment-strip thumbnail size.
const STRIP_THUMB_W: u32 = 104;
const STRIP_THUMB_H: u32 = 78;
/// Horizontal pitch between strip cells.
const STRIP_PITCH: i32 = 112;

/// One attached image. `bytes`/`preview` are `None` while the file read +
/// decode are in flight (camera attachments arrive fully-loaded).
struct Attachment {
    /// Match key for the in-flight `ReadImageFile` response (the file
    /// path), or a synthetic name for camera shots. Also the strip label.
    key: String,
    /// Upload-ready bytes (already run through `fit_for_upload`).
    bytes: Option<Vec<u8>>,
    mime: String,
    alt: String,
    /// Downscaled strip thumbnail.
    preview: Option<Texture>,
}

impl Attachment {
    fn loaded(&self) -> bool {
        self.bytes.is_some()
    }
}

/// One post within the thread being composed.
#[derive(Default)]
struct Segment {
    text: String,
    attachments: Vec<Attachment>,
}

impl Segment {
    /// A segment is postable if it has text or at least one (loaded)
    /// image and is within the length cap.
    fn valid(&self) -> bool {
        let n = self.text.chars().count();
        n <= POST_LIMIT
            && (n > 0 || !self.attachments.is_empty())
            && self.attachments.iter().all(|a| a.loaded())
    }
}

/// Bluesky's max posts in a thread.
const MAX_SEGMENTS: usize = 25;

/// Upload byte cap (bsky.social rejects larger image blobs). Images over
/// this are downscaled + re-encoded to JPEG before upload.
const MAX_UPLOAD_BYTES: usize = 1_000_000;
/// Longest-edge target when downscaling an oversized image.
const DOWNSCALE_EDGE: u32 = 1600;

/// Height of the close bar in the full-screen image view; the image is
/// fit + centered in the area below it so the bar never covers it.
const FULLVIEW_BAR_H: i32 = 30;
/// Height of the alt-text band at the bottom of the full-screen image
/// view. The image is decoded + centered to fit strictly between the top
/// bar and this band, so it never overlaps either.
const FULLVIEW_ALT_H: i32 = 56;

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
    /// Thread segments (≥1) — each is one post. `current` is the segment
    /// being edited.
    segments: Vec<Segment>,
    current: usize,
    state: ComposeState,
    cancel_btn: ButtonState,
    post_btn: ButtonState,
    text_area_btn: ButtonState,
    add_image_btn: ButtonState,
    camera_btn: ButtonState,
    prev_seg_btn: ButtonState,
    next_seg_btn: ButtonState,
    add_seg_btn: ButtonState,
    remove_seg_btn: ButtonState,
    /// Modal file picker; `Some` while the user is choosing an image.
    picker: Option<FilePicker>,
    /// Modal camera capture; `Some` while shooting a photo.
    camera: Option<CameraCapture>,
    /// Set when the camera modal finishes; the `CameraCapture` (and its
    /// textures) is dropped at the top of the NEXT frame after a
    /// `wait_rendering_done`, so its frame textures aren't freed in the
    /// frame they were drawn (GPU use-after-free).
    pending_camera_close: bool,
    /// Attachment index queued for removal from the current segment next
    /// frame (deferred so its thumbnail texture isn't freed in the frame
    /// it was drawn — GPU use-after-free).
    pending_remove: Option<usize>,
    /// Set when the file picker finishes; the `FilePicker` (and its
    /// thumbnail textures) is dropped at the top of the NEXT frame after a
    /// `wait_rendering_done`, so those textures aren't freed in the frame
    /// the picker just drew them (GPU use-after-free → GPUCRASH).
    pending_picker_close: bool,
    /// Per-strip-cell tap state (thumbnail body / remove).
    thumb_btns: [ButtonState; MAX_IMAGES],
    remove_btns: [ButtonState; MAX_IMAGES],
    /// Index of the attachment shown in the full-screen modal, if any.
    viewing: Option<usize>,
    /// Decoded screen-fit texture for the `viewing` attachment.
    full_tex: Option<Texture>,
    full_close_btn: ButtonState,
    edit_alt_btn: ButtonState,
    /// True while the IME is collecting alt text (vs. body text).
    editing_alt: bool,
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
            segments: vec![Segment::default()],
            current: 0,
            state: ComposeState::default(),
            cancel_btn: ButtonState::default(),
            post_btn: ButtonState::default(),
            text_area_btn: ButtonState::default(),
            add_image_btn: ButtonState::default(),
            camera_btn: ButtonState::default(),
            prev_seg_btn: ButtonState::default(),
            next_seg_btn: ButtonState::default(),
            add_seg_btn: ButtonState::default(),
            remove_seg_btn: ButtonState::default(),
            picker: None,
            camera: None,
            pending_camera_close: false,
            pending_remove: None,
            pending_picker_close: false,
            thumb_btns: Default::default(),
            remove_btns: Default::default(),
            viewing: None,
            full_tex: None,
            full_close_btn: ButtonState::default(),
            edit_alt_btn: ButtonState::default(),
            editing_alt: false,
            done: false,
        }
    }

    /// Char count of the segment currently being edited.
    fn char_count(&self) -> usize {
        self.segments[self.current].text.chars().count()
    }

    fn can_post(&self) -> bool {
        if !matches!(self.state, ComposeState::Editing | ComposeState::Error(_)) {
            return false;
        }
        // Every segment must be a valid, fully-loaded post.
        self.segments.iter().all(|s| s.valid())
    }

    /// Switch the segment being edited. No texture frees (the other
    /// segments keep their thumbnails); `full_tex` isn't drawn outside the
    /// modal so dropping it here is safe.
    fn switch_segment(&mut self, to: usize) {
        self.viewing = None;
        self.full_tex = None;
        self.current = to.min(self.segments.len().saturating_sub(1));
    }

    /// Remove the current segment (frees its thumbnail textures). Safe to
    /// free here: this runs in the thread bar, BEFORE the strip draws this
    /// frame, so the only in-flight references are last frame's — flushed
    /// by `wait_rendering_done`.
    fn remove_current_segment(&mut self) {
        if self.segments.len() <= 1 {
            return;
        }
        bsky_render::wait_rendering_done();
        self.viewing = None;
        self.full_tex = None;
        self.segments.remove(self.current);
        if self.current >= self.segments.len() {
            self.current = self.segments.len() - 1;
        }
    }

    /// Decode the screen-fit texture for the currently-`viewing`
    /// attachment of the current segment (reused while open).
    fn ensure_full_tex(&mut self) {
        if self.full_tex.is_some() {
            return;
        }
        let cur = self.current;
        if let Some(i) = self.viewing {
            if let Some(bytes) =
                self.segments[cur].attachments.get(i).and_then(|a| a.bytes.as_ref())
            {
                // CPU decode → small texture (never GPU-decode a local file).
                self.full_tex = decode_thumb(
                    bytes,
                    SCREEN_WIDTH as u32,
                    (SCREEN_HEIGHT - FULLVIEW_BAR_H - FULLVIEW_ALT_H) as u32,
                );
            }
        }
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

        // ─── 0a2. Deferred camera close: the result frame already drew
        //         the camera textures; wait for the GPU to consume that
        //         frame, then drop (free) them now that nothing references
        //         them. ────────────────────────────────────────────────
        if self.pending_camera_close {
            bsky_render::wait_rendering_done();
            self.camera = None;
            self.pending_camera_close = false;
        }

        // ─── 0a2b. Deferred picker close: the picker just drew its
        //          thumbnail textures in the frame it returned a result;
        //          wait for the GPU to consume that frame before dropping
        //          (freeing) them. ─────────────────────────────────────
        if self.pending_picker_close {
            bsky_render::wait_rendering_done();
            self.picker = None;
            self.pending_picker_close = false;
        }

        // ─── 0a3. Deferred attachment removal: its thumbnail was drawn
        //         last frame, so free it now (after the GPU consumed that
        //         frame), not in the frame the Remove button was tapped. ──
        if let Some(i) = self.pending_remove.take() {
            bsky_render::wait_rendering_done();
            let cur = self.current;
            if i < self.segments[cur].attachments.len() {
                self.segments[cur].attachments.remove(i);
            }
        }

        // ─── 0a4. Drain IME results (body OR alt text). Before the modals
        //         so it runs even while the full-view modal + alt IME are
        //         up. ──────────────────────────────────────────────────────
        match ime.poll() {
            ImeState::Finished(s) => {
                if self.editing_alt {
                    let cur = self.current;
                    if let Some(a) =
                        self.viewing.and_then(|i| self.segments[cur].attachments.get_mut(i))
                    {
                        a.alt = s;
                    }
                    self.editing_alt = false;
                } else {
                    self.segments[self.current].text = s;
                }
                ime.close();
            }
            ImeState::Cancelled | ImeState::Aborted => {
                self.editing_alt = false;
                ime.close();
            }
            _ => {}
        }

        // ─── 0b. Modal file picker takes over the whole frame. ─────────
        if self.picker.is_some() {
            match self.picker.as_mut().unwrap().render(frame, font, ctx) {
                Some(PickResult::Picked(path)) => {
                    // Append a loading attachment + request its bytes
                    // (matched back by `key` in handle_worker_response).
                    let cur = self.current;
                    if self.segments[cur].attachments.len() < MAX_IMAGES {
                        if let Some(worker) = ctx.worker {
                            worker.send(WorkRequest::ReadImageFile { path: path.clone() });
                        }
                        self.segments[cur].attachments.push(Attachment {
                            key: path,
                            bytes: None,
                            mime: String::new(),
                            alt: String::new(),
                            preview: None,
                        });
                    }
                    // Defer the drop: the picker drew its thumbnail
                    // textures this frame, so freeing them now would be a
                    // GPU use-after-free (GPUCRASH). Dropped next frame
                    // after wait_rendering_done (block 0a2b).
                    self.pending_picker_close = true;
                }
                Some(PickResult::Cancelled) => self.pending_picker_close = true,
                None => {}
            }
            return ScreenAction::None;
        }

        // ─── 0b2. Modal camera capture. ────────────────────────────────
        if self.camera.is_some() {
            match self.camera.as_mut().unwrap().render(frame, font, ctx) {
                Some(CameraResult::Confirmed(jpeg)) => {
                    // Camera JPEG is already small + in-spec; no fit needed.
                    let cur = self.current;
                    if self.segments[cur].attachments.len() < MAX_IMAGES {
                        let preview = decode_thumb(&jpeg, STRIP_THUMB_W, STRIP_THUMB_H);
                        self.segments[cur].attachments.push(Attachment {
                            key: "camera.jpg".to_string(),
                            bytes: Some(jpeg),
                            mime: "image/jpeg".to_string(),
                            alt: String::new(),
                            preview,
                        });
                    }
                    self.pending_camera_close = true;
                }
                Some(CameraResult::Cancelled) => self.pending_camera_close = true,
                None => {}
            }
            return ScreenAction::None;
        }

        // ─── 0c. Full-screen attachment preview + alt-text editing. ────
        if let Some(vi) = self.viewing {
            self.ensure_full_tex();
            let band_h = FULLVIEW_ALT_H;
            let band_y = SCREEN_HEIGHT - band_h;
            frame.fill_rect(
                0.0,
                0.0,
                SCREEN_WIDTH as f32,
                SCREEN_HEIGHT as f32,
                bsky_render::Color::rgb(0x00, 0x00, 0x00),
            );
            if let Some(tex) = &self.full_tex {
                // Center between the close bar (top) and the alt band.
                let avail_h = band_y - FULLVIEW_BAR_H;
                let dx = (SCREEN_WIDTH - tex.width()) / 2;
                let dy = FULLVIEW_BAR_H + (avail_h - tex.height()) / 2;
                frame.draw_texture(tex, dx as f32, dy as f32);
            }
            // Top bar.
            frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, FULLVIEW_BAR_H as f32, theme::FIELD_BG);
            frame.draw_text(font, 12, 21, theme::TEXT_PRIMARY, 0.9, "O / X  close");
            // Alt-text band + Edit button.
            let alt = self.segments[self.current]
                .attachments
                .get(vi)
                .map(|a| a.alt.clone())
                .unwrap_or_default();
            frame.fill_rect(0.0, band_y as f32, SCREEN_WIDTH as f32, band_h as f32, theme::FIELD_BG);
            let (alt_text, alt_color): (&str, _) = if alt.is_empty() {
                ("No alt text", theme::TEXT_MUTED)
            } else {
                (alt.as_str(), theme::TEXT_PRIMARY)
            };
            frame.draw_text_wrapped(font, 12, band_y + 20, SCREEN_WIDTH - 160, alt_color, 0.8, alt_text);
            let interactive = !ime.is_active();
            let edit_label = if alt.is_empty() { "Add alt" } else { "Edit alt" };
            if button(
                frame,
                font,
                Rect::new((SCREEN_WIDTH - 140) as f32, (band_y + 8) as f32, 130.0, 40.0),
                edit_label,
                &mut self.edit_alt_btn,
                ctx,
                interactive,
            ) {
                self.editing_alt = true;
                let _ = ime.open(
                    "Alt text",
                    ImeMode::Default,
                    TextBoxMode::Default,
                    ALT_LIMIT as u32,
                    &alt,
                );
            }
            // Close on a tap in the image area (above the alt band, below
            // the bar), or CIRCLE/CROSS — but only when the IME isn't up.
            if interactive {
                let in_image = ctx
                    .touches
                    .iter()
                    .any(|t| t.y > FULLVIEW_BAR_H && t.y < band_y);
                let tapped =
                    self.full_close_btn.pressed_last && !in_image && ctx.touches.is_empty();
                self.full_close_btn.pressed_last = in_image;
                if tapped
                    || ctx.pad.just_pressed(buttons::CIRCLE)
                    || ctx.pad.just_pressed(buttons::CROSS)
                {
                    // Keep full_tex alive (drawn this frame); freed on the
                    // next thumbnail-open or attachment removal.
                    self.viewing = None;
                    self.full_close_btn.pressed_last = false;
                }
            }
            return ScreenAction::None;
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

        // Post (right). "Post all" when it's a multi-segment thread.
        let post_rect = Rect::new(SCREEN_WIDTH as f32 - 108.0, 8.0, 100.0, 40.0);
        let can_post = self.can_post();
        let post_label = if self.segments.len() > 1 { "Post all" } else { "Post" };
        let post_clicked = button(
            frame,
            font,
            post_rect,
            post_label,
            &mut self.post_btn,
            ctx,
            can_post,
        );
        if post_clicked {
            if let Some(worker) = ctx.worker {
                // can_post guarantees every segment is valid + loaded.
                let segments: Vec<ThreadSegment> = self
                    .segments
                    .iter()
                    .map(|seg| ThreadSegment {
                        text: seg.text.clone(),
                        images: seg
                            .attachments
                            .iter()
                            .filter_map(|a| {
                                a.bytes.as_ref().map(|b| ComposedImage {
                                    bytes: b.clone(),
                                    mime: a.mime.clone(),
                                    alt: a.alt.clone(),
                                })
                            })
                            .collect(),
                    })
                    .collect();
                worker.send(WorkRequest::CreateThread {
                    segments,
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

        // ─── 4b. Thread bar: "Post N/M", segment nav, add/remove post. ─
        {
            let by = content_top;
            let nlen = self.segments.len();
            if nlen == 1 {
                // Not a thread yet — a single "+ Thread" entry; the nav /
                // add / delete controls stay hidden until threading starts.
                if button(frame, font, Rect::new(16.0, by as f32, 134.0, 32.0), "+ Thread", &mut self.add_seg_btn, ctx, interactive) {
                    self.segments.push(Segment::default());
                    self.switch_segment(1);
                }
            } else {
                // D-pad LEFT/RIGHT navigates segments too.
                if interactive {
                    if ctx.pad.just_pressed(buttons::LEFT) && self.current > 0 {
                        self.switch_segment(self.current - 1);
                    }
                    if ctx.pad.just_pressed(buttons::RIGHT) && self.current + 1 < nlen {
                        self.switch_segment(self.current + 1);
                    }
                }
                let label = format!("Post {} / {}", self.current + 1, nlen);
                frame.draw_text(font, 16, by + 23, theme::TEXT_PRIMARY, 0.9, &label);
                if button(frame, font, Rect::new(150.0, by as f32, 42.0, 32.0), "<", &mut self.prev_seg_btn, ctx, interactive && self.current > 0) {
                    self.switch_segment(self.current - 1);
                }
                if button(frame, font, Rect::new(198.0, by as f32, 42.0, 32.0), ">", &mut self.next_seg_btn, ctx, interactive && self.current + 1 < nlen) {
                    self.switch_segment(self.current + 1);
                }
                if button(frame, font, Rect::new(SCREEN_WIDTH as f32 - 232.0, by as f32, 112.0, 32.0), "+ Post", &mut self.add_seg_btn, ctx, interactive && nlen < MAX_SEGMENTS) {
                    self.segments.push(Segment::default());
                    self.switch_segment(self.segments.len() - 1);
                }
                if button(frame, font, Rect::new(SCREEN_WIDTH as f32 - 112.0, by as f32, 104.0, 32.0), "Del Post", &mut self.remove_seg_btn, ctx, interactive && nlen > 1) {
                    self.remove_current_segment();
                }
            }
            content_top += 40;
        }

        // The segment being edited (read after the thread bar may have
        // switched it).
        let cur = self.current;

        // ─── 5. Text area (tappable). ──────────────────────────────────
        // Reserve a strip below the text area for the image attachment
        // controls (Phase 7).
        let counter_h = 32;
        // Strip is a thumbnail row when images are attached; a single
        // Add/Camera button row otherwise (no dead space).
        let attach_h = if self.segments[cur].attachments.is_empty() { 44 } else { 94 };
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
        let empty = self.segments[cur].text.is_empty();
        let body = if empty {
            if self.reply_to.is_some() {
                "Tap to write your reply…"
            } else {
                "Tap to write your post…"
            }
        } else {
            self.segments[cur].text.as_str()
        };
        let body_color = if empty {
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
                &self.segments[cur].text,
            );
        }

        // ─── 5b. Attachment strip: thumbnails (+ALT/✕) then Add/Camera. ─
        let attach_y = content_top + area_h + 8;
        let tw = STRIP_THUMB_W as i32;
        let th = STRIP_THUMB_H as i32;
        let mut open_view: Option<usize> = None;
        let mut request_remove: Option<usize> = None;
        let n_att = self.segments[cur].attachments.len();
        for i in 0..n_att {
            let cell_x = 12 + (i as i32) * STRIP_PITCH;
            // Thumbnail (or a loading placeholder).
            if let Some(tex) = self.segments[cur].attachments[i].preview.as_ref() {
                frame.draw_texture(tex, cell_x as f32, attach_y as f32);
            } else {
                frame.fill_rect(cell_x as f32, attach_y as f32, tw as f32, th as f32, theme::FIELD_BG);
                frame.draw_text(font, cell_x + 8, attach_y + th / 2, theme::TEXT_MUTED, 0.8, "…");
            }
            // ALT badge.
            if !self.segments[cur].attachments[i].alt.is_empty() {
                frame.fill_rect(cell_x as f32, (attach_y + th - 18) as f32, 36.0, 18.0, theme::ACCENT);
                frame.draw_text(font, cell_x + 5, attach_y + th - 4, theme::TEXT_PRIMARY, 0.62, "ALT");
            }
            // Remove (top-right).
            let rm_rect = Rect::new((cell_x + tw - 26) as f32, attach_y as f32, 26.0, 26.0);
            if button(frame, font, rm_rect, "x", &mut self.remove_btns[i], ctx, interactive) {
                request_remove = Some(i);
            }
            // Tap the thumbnail body (below the remove corner) → full view.
            let body_rect = Rect::new(cell_x as f32, (attach_y + 26) as f32, tw as f32, (th - 26) as f32);
            if self.segments[cur].attachments[i].preview.is_some()
                && button_invisible(frame, body_rect, &mut self.thumb_btns[i], ctx, interactive)
            {
                open_view = Some(i);
            }
        }
        // Add / Camera while under the cap.
        if n_att < MAX_IMAGES {
            let bx = 12 + (n_att as i32) * STRIP_PITCH;
            let by = if n_att == 0 { attach_y } else { attach_y + 19 };
            if button(frame, font, Rect::new(bx as f32, by as f32, 72.0, 40.0), "Add", &mut self.add_image_btn, ctx, interactive) {
                self.picker = Some(FilePicker::new());
            }
            if button(frame, font, Rect::new((bx + 80) as f32, by as f32, 100.0, 40.0), "Camera", &mut self.camera_btn, ctx, interactive) {
                self.camera = Some(CameraCapture::new());
            }
        }
        if let Some(i) = request_remove {
            self.pending_remove = Some(i);
        }
        if let Some(i) = open_view {
            // Switching attachments: drop the (not-drawn-this-frame) old
            // full_tex so ensure_full_tex re-decodes for `i`.
            self.full_tex = None;
            self.viewing = Some(i);
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
            // Picker thumbnail (if the picker is open).
            if let Some(picker) = self.picker.as_mut() {
                picker.on_image(url, bytes);
            }
            // Bytes for a loading attachment (matched by key) → fit +
            // decode its strip thumbnail.
            if let Ok(b) = bytes {
                // Match across ALL segments — the user may switch segments
                // while an image is still loading.
                if let Some(att) = self
                    .segments
                    .iter_mut()
                    .flat_map(|s| s.attachments.iter_mut())
                    .find(|a| !a.loaded() && a.key == *url)
                {
                    let (upload_bytes, mime) = fit_for_upload(b, url);
                    att.preview = decode_thumb(&upload_bytes, STRIP_THUMB_W, STRIP_THUMB_H);
                    att.bytes = Some(upload_bytes);
                    att.mime = mime;
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
                    self.segments = vec![Segment::default()];
                    self.current = 0;
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
    // Decode + downscale on the CPU — GPU-decoding a multi-MB image
    // crashes vita2d (see bsky_media::image).
    let Ok((rgba, w, h)) = bsky_media::image::decode_rgba(raw) else {
        return (raw.to_vec(), mime_from_path(path));
    };
    let (rgba, w, h) =
        bsky_media::image::downscale_rgba(rgba, w, h, DOWNSCALE_EDGE, DOWNSCALE_EDGE);
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

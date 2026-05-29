//! Fullscreen video player screen — Phase 5.3.
//!
//! State machine:
//!
//! ```text
//!   Pending
//!     │ first frame
//!     ▼
//!   Downloading ─── WorkResponse::VideoBlob(Ok(path)) ─▶ Ready
//!     │                                                   │
//!     │ Err                                               │ next frame
//!     ▼                                                   ▼
//!   Error                                              Playing
//! ```
//!
//! Inputs (Playing state):
//!
//! - CIRCLE → `ScreenAction::Pop`
//! - SQUARE → toggle pause / resume
//! - D-pad LEFT / RIGHT → seek -10 s / +10 s (clamped to start)
//! - Touch tap anywhere → toggle transport-overlay visibility (auto-hides
//!   3 s after the last input).

use std::sync::Arc;

use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_input::buttons;
use bsky_render::{theme, Font, Frame, Texture, YuvTexture, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_video::{AudioOut, PlayerState, VideoPlayer};
use bsky_worker::{WorkRequest, WorkResponse};

use crate::screen::{Screen, ScreenAction};
use crate::widget::{button, ButtonState, Rect, UiCtx};

const SEEK_STEP_US: u64 = 10_000_000; // 10 seconds in microseconds
/// Auto-hide the transport overlay this many frames after the last
/// input (60 fps × 3 s).
const OVERLAY_HIDE_FRAMES: u32 = 180;

/// Marker file whose presence means the user ticked "Don't show this
/// again" on the greyscale-video notice. Lives in the app data dir
/// alongside the session files.
fn notice_flag_path() -> String {
    format!("{}/greyscale_notice_dismissed", bsky_auth::DATA_DIR)
}

/// Whether the user has previously suppressed the greyscale notice.
fn notice_was_dismissed() -> bool {
    std::fs::metadata(notice_flag_path()).is_ok()
}

/// Persist the "don't show again" choice (best-effort; a failed write
/// just means the notice shows again next time).
fn persist_notice_dismissed() {
    if let Err(e) = std::fs::write(notice_flag_path(), b"") {
        bsky_log::log!("greyscale notice: persist failed: {e}");
    }
}

enum VideoState {
    /// First-frame dispatch hasn't fired yet.
    Pending,
    /// `FetchVideoBlob` in flight.
    Downloading,
    /// File is on disk; the player isn't open yet (open happens on
    /// the next frame to keep one heavy operation per tick).
    Ready { file_path: String },
    Playing {
        player: VideoPlayer,
        audio: AudioOut,
        /// Color path: three luma textures + GXM YUV→RGB shader. `None`
        /// in greyscale mode.
        tex: Option<YuvTexture>,
        /// Greyscale fallback: a single RGBA8 texture (Y spread to
        /// R=G=B) drawn with vita2d's stock shader — no
        /// `libshacccg.suprx` needed. `None` in color mode.
        grey_tex: Option<Texture>,
        /// Decided once at playback start: `true` when the console lacks
        /// the shader compiler and we render greyscale.
        greyscale: bool,
        // Cached so we don't re-create the texture every frame.
        tex_w: u32,
        tex_h: u32,
    },
    Error(String),
}

pub struct VideoPlayerScreen {
    /// Held for symmetry with other screens; future overlays (e.g.
    /// tap-author-while-video-paused) can use the agent.
    #[allow(dead_code)]
    client: Arc<AuthClient>,
    did: String,
    cid: String,
    state: VideoState,
    /// Frames since the last input. When >= OVERLAY_HIDE_FRAMES, the
    /// overlay is hidden.
    overlay_idle: u32,
    /// Greyscale-fallback notice modal: armed when playback starts
    /// without color support and the user hasn't previously ticked
    /// "Don't show this again".
    show_notice: bool,
    /// Live state of the "Don't show this again" checkbox.
    notice_dont_show: bool,
    notice_ok_btn: ButtonState,
    notice_check: ButtonState,
}

impl VideoPlayerScreen {
    pub fn new(client: Arc<AuthClient>, did: String, cid: String) -> Self {
        Self {
            client,
            did,
            cid,
            state: VideoState::Pending,
            overlay_idle: 0,
            show_notice: false,
            notice_dont_show: false,
            notice_ok_btn: ButtonState::default(),
            notice_check: ButtonState::default(),
        }
    }

    fn show_overlay(&self) -> bool {
        self.overlay_idle < OVERLAY_HIDE_FRAMES
    }

    /// Close the greyscale notice, persisting the suppression flag if the
    /// "Don't show this again" box is ticked. Routed from the Got-it
    /// button, a CROSS press, or a CIRCLE press while the notice is up.
    fn dismiss_notice(&mut self) {
        if self.notice_dont_show {
            persist_notice_dismissed();
        }
        self.show_notice = false;
    }

    /// Draw the greyscale-fallback dialog over the (live) video and route
    /// its input. Called last in `frame()` so it composites on top.
    fn draw_greyscale_notice(&mut self, frame: &mut Frame, font: &Font, ctx: &UiCtx) {
        let pw = 640.0_f32;
        let ph = 252.0_f32;
        let px = (SCREEN_WIDTH as f32 - pw) / 2.0;
        let py = (SCREEN_HEIGHT as f32 - ph) / 2.0;

        // 2px ACCENT border, then the FIELD_BG panel inside it. No alpha
        // dim — the greyscale video stays visible around the dialog.
        frame.fill_rect(px - 2.0, py - 2.0, pw + 4.0, ph + 4.0, theme::ACCENT);
        frame.fill_rect(px, py, pw, ph, theme::FIELD_BG);

        let pad = 24;
        frame.draw_text(
            font,
            px as i32 + pad,
            py as i32 + 44,
            theme::TEXT_PRIMARY,
            1.25,
            "Playing in greyscale",
        );
        frame.draw_text_wrapped(
            font,
            px as i32 + pad,
            py as i32 + 78,
            pw as i32 - pad * 2,
            theme::TEXT_MUTED,
            0.95,
            "Color video needs the system shader module (libshacccg.suprx), \
             which isn't installed on this console. Audio and greyscale video \
             work fine without it.",
        );

        // Checkbox. The hit-rect spans the box + its label so either is
        // tappable; a clean tap (down-inside, up-with-no-touch) toggles.
        let box_sz = 26.0_f32;
        let box_x = px + pad as f32;
        let box_y = py + ph - 102.0;
        let check_rect = Rect::new(box_x, box_y - 4.0, 300.0, box_sz + 8.0);
        let pressed_now = ctx.touches.iter().any(|t| check_rect.contains(t.x, t.y));
        let toggled =
            self.notice_check.pressed_last && !pressed_now && ctx.touches.is_empty();
        self.notice_check.pressed_last = pressed_now;
        if toggled {
            self.notice_dont_show = !self.notice_dont_show;
        }
        // Box: muted border, BACKGROUND interior, ACCENT fill when ticked.
        frame.fill_rect(box_x, box_y, box_sz, box_sz, theme::TEXT_MUTED);
        frame.fill_rect(box_x + 2.0, box_y + 2.0, box_sz - 4.0, box_sz - 4.0, theme::BACKGROUND);
        if self.notice_dont_show {
            frame.fill_rect(box_x + 6.0, box_y + 6.0, box_sz - 12.0, box_sz - 12.0, theme::ACCENT);
        }
        frame.draw_text(
            font,
            (box_x + box_sz + 12.0) as i32,
            (box_y + box_sz - 5.0) as i32,
            theme::TEXT_PRIMARY,
            0.95,
            "Don't show this again",
        );

        // Got-it button + CROSS/CIRCLE all dismiss.
        let ok_rect = Rect::new(px + pw - 160.0, py + ph - 60.0, 136.0, 40.0);
        let ok_clicked = button(frame, font, ok_rect, "Got it", &mut self.notice_ok_btn, ctx, true);
        if ok_clicked || ctx.pad.just_pressed(buttons::CROSS) {
            self.dismiss_notice();
        }
    }
}

impl Screen for VideoPlayerScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        _ime: &mut Ime,
    ) -> ScreenAction {
        // ─── Input handling ──────────────────────────────────────────
        if ctx.pad.just_pressed(buttons::CIRCLE) {
            if self.show_notice {
                // CIRCLE dismisses the greyscale dialog rather than
                // backing out of the video entirely.
                self.dismiss_notice();
            } else {
                return ScreenAction::Pop;
            }
        }
        let any_input = ctx.pad.current != ctx.pad.previous || !ctx.touches.is_empty();
        if any_input {
            self.overlay_idle = 0;
        } else {
            self.overlay_idle = self.overlay_idle.saturating_add(1);
        }

        // ─── State transitions ──────────────────────────────────────
        if matches!(self.state, VideoState::Pending) {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::FetchVideoBlob {
                    did: self.did.clone(),
                    cid: self.cid.clone(),
                });
                self.state = VideoState::Downloading;
            }
        }
        if let VideoState::Ready { file_path } = &self.state {
            // Open player + audio + texture on this frame; transition.
            let path = file_path.clone();
            match VideoPlayer::open(&path) {
                Ok(player) => {
                    // Probe color support once (triggers the one-shot
                    // shader compile). Without libshacccg.suprx we render
                    // greyscale and surface the one-time notice.
                    let greyscale = !bsky_render::video_color_supported();
                    if greyscale && !notice_was_dismissed() {
                        self.show_notice = true;
                    }
                    self.state = VideoState::Playing {
                        player,
                        audio: AudioOut::new(),
                        tex: None,
                        grey_tex: None,
                        greyscale,
                        tex_w: 0,
                        tex_h: 0,
                    };
                }
                Err(e) => {
                    bsky_log::log!("VideoPlayer::open failed for {path}: {e}");
                    self.state = VideoState::Error(format!("{e}"));
                }
            }
        }

        // Transport controls are suppressed while the greyscale notice is
        // up (so the dismissing tap/press doesn't also seek or pause).
        let notice_up = self.show_notice;

        // Playing-state input handling + frame pulls.
        if let VideoState::Playing { player, audio, tex, grey_tex, greyscale, tex_w, tex_h } =
            &mut self.state
        {
            if !notice_up && ctx.pad.just_pressed(buttons::SQUARE) {
                match player.state() {
                    PlayerState::Playing => player.pause(),
                    PlayerState::Paused => player.resume(),
                    PlayerState::Eof => {
                        // Seek to start and resume from EOF.
                        player.jump_to_time_us(0);
                        player.resume();
                    }
                }
            }
            if !notice_up && ctx.pad.just_pressed(buttons::LEFT) {
                let now = player.current_time_us();
                let target = now.saturating_sub(SEEK_STEP_US);
                player.jump_to_time_us(target);
            }
            if !notice_up && ctx.pad.just_pressed(buttons::RIGHT) {
                let now = player.current_time_us();
                player.jump_to_time_us(now + SEEK_STEP_US);
            }

            // Pull the latest video frame (if any). On size change,
            // (re)create the texture for the active render path.
            if let Some(yuv) = player.next_video_frame() {
                let size_changed = *tex_w != yuv.width || *tex_h != yuv.height;
                if *greyscale {
                    // Greyscale: single RGBA8 texture, stock shader.
                    if size_changed || grey_tex.is_none() {
                        // Sync the GPU before dropping the old texture on a
                        // size change (GPU defer-drop rule).
                        if size_changed && grey_tex.is_some() {
                            bsky_render::wait_rendering_done();
                        }
                        match Texture::new_luma(yuv.width, yuv.height) {
                            Ok(t) => {
                                *grey_tex = Some(t);
                                *tex_w = yuv.width;
                                *tex_h = yuv.height;
                            }
                            Err(e) => {
                                bsky_log::log!("grey video texture create failed: {e}");
                            }
                        }
                    }
                    if let Some(t) = grey_tex.as_ref() {
                        // Sync before overwriting texture data the GPU may
                        // still be sampling from last frame (tearing on
                        // motion) — mirrors YuvTexture::upload.
                        bsky_render::wait_rendering_done();
                        t.upload_luma(yuv.y, yuv.y_pitch);
                    }
                } else {
                    // Color: three luma textures + GXM YUV→RGB shader.
                    if size_changed || tex.is_none() {
                        match YuvTexture::create(yuv.width, yuv.height) {
                            Ok(t) => {
                                *tex = Some(t);
                                *tex_w = yuv.width;
                                *tex_h = yuv.height;
                            }
                            Err(e) => {
                                bsky_log::log!("YUV texture create failed: {e}");
                            }
                        }
                    }
                    if let Some(t) = tex.as_mut() {
                        t.upload(yuv.y, yuv.y_pitch, yuv.uv, yuv.uv_pitch);
                    }
                }
            }
            // Pull a small batch of audio chunks per render frame.
            // `audio.write` blocks until the audio chip drains the
            // previous buffer — draining unboundedly here starves the
            // render thread (controls lock up; eventually crash).
            // Two chunks ≈ ~40 ms of audio, leaves plenty of headroom
            // for the next 16 ms render budget.
            for _ in 0..2 {
                let Some(chunk) = player.next_audio_samples() else { break };
                if let Err(e) = audio.write(&chunk) {
                    bsky_log::log!("audio write failed: {e}");
                    break;
                }
            }
        }

        // ─── Render ─────────────────────────────────────────────────
        // Background fill (letterbox bars stay this color).
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, SCREEN_HEIGHT as f32, theme::BACKGROUND);

        match &self.state {
            VideoState::Pending | VideoState::Downloading => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2,
                    theme::TEXT_PRIMARY,
                    1.1,
                    "Loading video…",
                );
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 + 28,
                    theme::TEXT_MUTED,
                    0.85,
                    "CIRCLE to cancel",
                );
            }
            VideoState::Ready { .. } => {
                // One-frame interim — open() will run next frame.
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2,
                    theme::TEXT_PRIMARY,
                    1.0,
                    "Opening…",
                );
            }
            VideoState::Playing { player, tex, grey_tex, greyscale, tex_w, tex_h, .. } => {
                let (s, _sy, dx, dy, dw, dh) = aspect_fit(*tex_w, *tex_h);
                if *greyscale {
                    // No-shader fallback: RGBA8 luma texture (Y spread to
                    // R=G=B) drawn with vita2d's stock shader, aspect-fit.
                    if let Some(t) = grey_tex.as_ref() {
                        frame.draw_texture_scale(t, dx, dy, s, s);
                    }
                } else if let Some(t) = tex.as_ref() {
                    // Custom GXM YUV→RGB shader (Phase 5.3.x.1):
                    // three luma textures → fragment shader applies
                    // BT.601 limited-range matrix → opaque RGBA.
                    frame.draw_video_yuv(t, dx, dy, dw, dh);
                }
                if self.show_overlay() {
                    draw_transport(frame, font, player);
                }
            }
            VideoState::Error(msg) => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 - 20,
                    theme::ERROR,
                    1.1,
                    "Couldn't play video",
                );
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 + 16,
                    theme::TEXT_MUTED,
                    0.85,
                    msg,
                );
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 + 44,
                    theme::TEXT_MUTED,
                    0.85,
                    "CIRCLE to go back",
                );
            }
        }

        // Greyscale-fallback notice composites on top of everything.
        if self.show_notice {
            self.draw_greyscale_notice(frame, font, ctx);
        }

        ScreenAction::None
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        if let WorkResponse::VideoBlob { cid, result } = resp {
            if cid != self.cid {
                return; // Not for us.
            }
            self.state = match result {
                Ok(file_path) => VideoState::Ready { file_path },
                Err(e) => VideoState::Error(e),
            };
        }
    }
}

/// Compute (sx, sy, dx, dy, dw, dh) for aspect-fit drawing of a
/// `width × height` texture into a 960×544 viewport. Letterbox bars
/// fill leftover space (rendered before the texture as `BACKGROUND`).
fn aspect_fit(width: u32, height: u32) -> (f32, f32, f32, f32, f32, f32) {
    if width == 0 || height == 0 {
        return (1.0, 1.0, 0.0, 0.0, 0.0, 0.0);
    }
    let sw = SCREEN_WIDTH as f32;
    let sh = SCREEN_HEIGHT as f32;
    let tw = width as f32;
    let th = height as f32;
    let s = (sw / tw).min(sh / th);
    let dw = tw * s;
    let dh = th * s;
    let dx = (sw - dw) / 2.0;
    let dy = (sh - dh) / 2.0;
    (s, s, dx, dy, dw, dh)
}

fn draw_transport(frame: &mut Frame, font: &Font, player: &VideoPlayer) {
    const BAR_H: i32 = 36;
    let bar_y = SCREEN_HEIGHT - BAR_H;
    // Translucent background bar (no real alpha — matches FIELD_BG).
    frame.fill_rect(0.0, bar_y as f32, SCREEN_WIDTH as f32, BAR_H as f32, theme::FIELD_BG);
    let now = player.current_time_us();
    let label = format!(
        "{}  ·  {}",
        format_us(now),
        match player.state() {
            PlayerState::Playing => "playing",
            PlayerState::Paused => "paused",
            PlayerState::Eof => "ended",
        }
    );
    frame.draw_text(
        font,
        12,
        bar_y + 24,
        theme::TEXT_PRIMARY,
        0.95,
        &label,
    );
    let hint = "SQUARE pause   ◄ ► seek 10s   CIRCLE back";
    let (hw, _) = frame.measure_text(font, 0.85, hint);
    frame.draw_text(
        font,
        SCREEN_WIDTH - hw - 12,
        bar_y + 24,
        theme::TEXT_MUTED,
        0.85,
        hint,
    );
}

fn format_us(us: u64) -> String {
    let total_secs = us / 1_000_000;
    let m = total_secs / 60;
    let s = total_secs % 60;
    format!("{m}:{s:02}")
}

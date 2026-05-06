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
use bsky_render::{theme, Font, Frame, YuvTexture, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_video::{AudioOut, PlayerState, VideoPlayer};
use bsky_worker::{WorkRequest, WorkResponse};

use crate::screen::{Screen, ScreenAction};
use crate::widget::UiCtx;

const SEEK_STEP_US: u64 = 10_000_000; // 10 seconds in microseconds
/// Auto-hide the transport overlay this many frames after the last
/// input (60 fps × 3 s).
const OVERLAY_HIDE_FRAMES: u32 = 180;

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
        tex: Option<YuvTexture>,
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
}

impl VideoPlayerScreen {
    pub fn new(client: Arc<AuthClient>, did: String, cid: String) -> Self {
        Self {
            client,
            did,
            cid,
            state: VideoState::Pending,
            overlay_idle: 0,
        }
    }

    fn show_overlay(&self) -> bool {
        self.overlay_idle < OVERLAY_HIDE_FRAMES
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
            return ScreenAction::Pop;
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
                    self.state = VideoState::Playing {
                        player,
                        audio: AudioOut::new(),
                        tex: None,
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

        // Playing-state input handling + frame pulls.
        if let VideoState::Playing { player, audio, tex, tex_w, tex_h } = &mut self.state {
            if ctx.pad.just_pressed(buttons::SQUARE) {
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
            if ctx.pad.just_pressed(buttons::LEFT) {
                let now = player.current_time_us();
                let target = now.saturating_sub(SEEK_STEP_US);
                player.jump_to_time_us(target);
            }
            if ctx.pad.just_pressed(buttons::RIGHT) {
                let now = player.current_time_us();
                player.jump_to_time_us(now + SEEK_STEP_US);
            }

            // Pull the latest video frame (if any). On size change,
            // (re)create the texture.
            if let Some(yuv) = player.next_video_frame() {
                if *tex_w != yuv.width || *tex_h != yuv.height || tex.is_none() {
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
            VideoState::Playing { player, tex, tex_w, tex_h, .. } => {
                if let Some(t) = tex.as_ref() {
                    let (_sx, _sy, dx, dy, dw, dh) =
                        aspect_fit(*tex_w, *tex_h);
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

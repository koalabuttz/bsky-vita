//! Camera capture component (live preview → shoot → confirm/retake).
//!
//! A reusable component embedded modally in ComposeScreen (like
//! `FilePicker`). Opens the back camera on construct; on confirm it
//! encodes the frozen frame to JPEG (`bsky_media::jpeg`) and returns the
//! bytes for the attachment pipeline.
//!
//! Controls: SQUARE shoot · TRIANGLE flip front/back · CIRCLE back ·
//! (captured) CROSS use photo · CIRCLE/SQUARE retake.

use bsky_input::buttons;
use bsky_media::camera::{Camera, CAM_H, CAM_W, DEVICE_BACK, DEVICE_FRONT};
use bsky_media::jpeg;
use bsky_render::{theme, Color, Font, Frame, Texture, SCREEN_HEIGHT, SCREEN_WIDTH};

use crate::widget::UiCtx;

const BAR_H: i32 = 30;

pub enum CameraResult {
    /// Confirmed photo, encoded to JPEG bytes.
    Confirmed(Vec<u8>),
    Cancelled,
}

enum Mode {
    Live,
    Captured,
}

pub struct CameraCapture {
    camera: Option<Camera>,
    device: i32,
    /// Reused live-preview texture.
    tex: Option<Texture>,
    /// Captured frame (RGBA) + its texture.
    frozen: Option<Vec<u8>>,
    frozen_tex: Option<Texture>,
    mode: Mode,
    error: Option<String>,
}

impl CameraCapture {
    pub fn new() -> Self {
        let device = DEVICE_BACK;
        let (camera, error) = match Camera::open(device) {
            Ok(c) => (Some(c), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            camera,
            device,
            tex: None,
            frozen: None,
            frozen_tex: None,
            mode: Mode::Live,
            error,
        }
    }

    fn flip(&mut self) {
        self.device = if self.device == DEVICE_BACK {
            DEVICE_FRONT
        } else {
            DEVICE_BACK
        };
        // Drop the old camera (stop+close+free DMA buffer — not a GPU
        // texture, safe mid-frame). Keep `tex` (same 640×480 size) to
        // reuse it for the new camera; freeing it here would be a
        // use-after-free since it was drawn this frame.
        self.camera = None;
        match Camera::open(self.device) {
            Ok(c) => {
                self.camera = Some(c);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn render(&mut self, frame: &mut Frame, font: &Font, ctx: &UiCtx) -> Option<CameraResult> {
        frame.fill_rect(
            0.0,
            0.0,
            SCREEN_WIDTH as f32,
            SCREEN_HEIGHT as f32,
            Color::rgb(0x00, 0x00, 0x00),
        );

        // ── Error state ────────────────────────────────────────────────
        if let Some(err) = &self.error {
            let msg = format!("Camera unavailable: {err}");
            frame.draw_text_centered(font, SCREEN_HEIGHT / 2, theme::ERROR, 0.95, &msg);
            frame.draw_text_centered(
                font,
                SCREEN_HEIGHT / 2 + 30,
                theme::TEXT_MUTED,
                0.85,
                "Press O to go back",
            );
            if ctx.pad.just_pressed(buttons::CIRCLE) {
                return Some(CameraResult::Cancelled);
            }
            return None;
        }

        match self.mode {
            Mode::Live => {
                // Pull a frame into the (lazily-created) preview texture.
                if self.tex.is_none() {
                    self.tex = Texture::new_rgba(CAM_W, CAM_H).ok();
                }
                let mut snapshot: Option<Vec<u8>> = None;
                if let (Some(cam), Some(tex)) = (self.camera.as_mut(), self.tex.as_ref()) {
                    if let Some(rgba) = cam.read_rgba() {
                        tex.upload_rgba(rgba);
                        if ctx.pad.just_pressed(buttons::SQUARE) {
                            snapshot = Some(rgba.to_vec());
                        }
                    }
                }
                if let Some(tex) = &self.tex {
                    draw_fit(frame, tex);
                }
                // Capture → freeze. Reuse the frozen texture if present
                // (same size) rather than recreating — avoids churn.
                if let Some(snap) = snapshot {
                    if self.frozen_tex.is_none() {
                        self.frozen_tex = Texture::new_rgba(CAM_W, CAM_H).ok();
                    }
                    if let Some(ft) = &self.frozen_tex {
                        ft.upload_rgba(&snap);
                    }
                    self.frozen = Some(snap);
                    self.mode = Mode::Captured;
                }

                bar(frame, font, "SQUARE shoot    TRIANGLE flip    O back");
                if ctx.pad.just_pressed(buttons::TRIANGLE) {
                    self.flip();
                }
                if ctx.pad.just_pressed(buttons::CIRCLE) {
                    return Some(CameraResult::Cancelled);
                }
            }
            Mode::Captured => {
                if let Some(ft) = &self.frozen_tex {
                    draw_fit(frame, ft);
                }
                bar(frame, font, "X use photo    O / SQUARE retake");
                if ctx.pad.just_pressed(buttons::CROSS) {
                    if let Some(rgba) = self.frozen.take() {
                        match jpeg::encode_rgba(&rgba, CAM_W, CAM_H, 85) {
                            Ok(jpeg) => return Some(CameraResult::Confirmed(jpeg)),
                            Err(e) => self.error = Some(format!("encode: {e}")),
                        }
                    }
                }
                if ctx.pad.just_pressed(buttons::CIRCLE) || ctx.pad.just_pressed(buttons::SQUARE) {
                    // Keep frozen_tex (drawn this frame; reused next
                    // capture) — don't free it mid-frame.
                    self.frozen = None;
                    self.mode = Mode::Live;
                }
            }
        }
        None
    }
}

impl Default for CameraCapture {
    fn default() -> Self {
        Self::new()
    }
}

/// Draw a 640×480 frame texture scaled-to-fit + centered below the bar.
fn draw_fit(frame: &mut Frame, tex: &Texture) {
    let avail_h = SCREEN_HEIGHT - BAR_H;
    let scale = (SCREEN_WIDTH as f32 / CAM_W as f32).min(avail_h as f32 / CAM_H as f32);
    let dw = (CAM_W as f32 * scale) as i32;
    let dh = (CAM_H as f32 * scale) as i32;
    let x = (SCREEN_WIDTH - dw) / 2;
    let y = BAR_H + (avail_h - dh) / 2;
    frame.draw_texture_scale(tex, x as f32, y as f32, scale, scale);
}

fn bar(frame: &mut Frame, font: &Font, hint: &str) {
    frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, BAR_H as f32, theme::FIELD_BG);
    frame.draw_text(font, 12, 21, theme::TEXT_PRIMARY, 0.9, hint);
}

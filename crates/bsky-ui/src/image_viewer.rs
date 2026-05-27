//! Full-screen image viewer for feed/thread embed images.
//!
//! Pushed onto the nav stack when an image in a post is tapped. Shows the
//! `fullsize` CDN image (decoded into the shared `TextureCache` via the
//! normal `FetchImage` pipeline), falling back to the already-cached
//! `thumb` while it loads. Alt text shows in a bottom band; D-pad
//! LEFT/RIGHT pages through a multi-image post; CIRCLE / tap closes.

use bsky_ime::Ime;
use bsky_input::buttons;
use bsky_render::{theme, Color, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{WorkRequest, WorkResponse};

use crate::embeds::ViewerImage;
use crate::screen::{Screen, ScreenAction};
use crate::widget::UiCtx;

const BAR_H: i32 = 30;
const ALT_BAND_H: i32 = 44;

pub struct ImageViewerScreen {
    images: Vec<ViewerImage>,
    index: usize,
    /// fullsize URL currently being fetched (suppresses re-dispatch).
    inflight: Option<String>,
    /// Release-edge state for tap-to-close.
    close_pressed_last: bool,
    /// Tap-to-close is armed only after the touch that OPENED the viewer
    /// (the feed opens on press, so the finger is still down) has lifted —
    /// otherwise that same touch's release would immediately close it.
    armed: bool,
}

impl ImageViewerScreen {
    pub fn new(images: Vec<ViewerImage>, index: usize) -> Self {
        let index = index.min(images.len().saturating_sub(1));
        Self {
            images,
            index,
            inflight: None,
            close_pressed_last: false,
            armed: false,
        }
    }
}

impl Screen for ImageViewerScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        _ime: &mut Ime,
    ) -> ScreenAction {
        frame.fill_rect(
            0.0,
            0.0,
            SCREEN_WIDTH as f32,
            SCREEN_HEIGHT as f32,
            Color::rgb(0x00, 0x00, 0x00),
        );
        if self.images.is_empty() {
            return ScreenAction::Pop;
        }
        let img = &self.images[self.index];

        // Fetch the fullsize if not cached / not already requested.
        if !ctx.texture_cache.contains(&img.fullsize)
            && self.inflight.as_deref() != Some(img.fullsize.as_str())
        {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::FetchImage {
                    url: img.fullsize.clone(),
                });
                self.inflight = Some(img.fullsize.clone());
            }
        }

        let band_h = if img.alt.is_empty() { 0 } else { ALT_BAND_H };
        let img_bottom = SCREEN_HEIGHT - band_h;

        // Draw the fullsize if ready, else the (already-cached) thumb.
        let tex = ctx
            .texture_cache
            .get(&img.fullsize)
            .or_else(|| ctx.texture_cache.get(&img.thumb));
        if let Some(tex) = tex {
            let avail_h = img_bottom - BAR_H;
            let tw = tex.width().max(1) as f32;
            let th = tex.height().max(1) as f32;
            let s = (SCREEN_WIDTH as f32 / tw).min(avail_h as f32 / th);
            let dw = tw * s;
            let dh = th * s;
            let dx = (SCREEN_WIDTH as f32 - dw) / 2.0;
            let dy = BAR_H as f32 + (avail_h as f32 - dh) / 2.0;
            frame.draw_texture_scale(tex, dx, dy, s, s);
        } else {
            frame.draw_text_centered(font, SCREEN_HEIGHT / 2, theme::TEXT_MUTED, 0.9, "Loading…");
        }

        // Top bar: page indicator + close hint.
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, BAR_H as f32, theme::FIELD_BG);
        let hint = if self.images.len() > 1 {
            format!("{} / {}    O / X close    ← → switch", self.index + 1, self.images.len())
        } else {
            "O / X close".to_string()
        };
        frame.draw_text(font, 12, 21, theme::TEXT_PRIMARY, 0.9, &hint);

        // Alt-text band.
        if band_h > 0 {
            frame.fill_rect(0.0, img_bottom as f32, SCREEN_WIDTH as f32, band_h as f32, theme::FIELD_BG);
            frame.draw_text_wrapped(
                font,
                12,
                img_bottom + 20,
                SCREEN_WIDTH - 24,
                theme::TEXT_PRIMARY,
                0.8,
                &img.alt,
            );
        }

        // D-pad pages through a multi-image post.
        if self.images.len() > 1 {
            if ctx.pad.just_pressed(buttons::LEFT) && self.index > 0 {
                self.index -= 1;
            }
            if ctx.pad.just_pressed(buttons::RIGHT) && self.index + 1 < self.images.len() {
                self.index += 1;
            }
        }

        // Close: CIRCLE/CROSS, or a fresh tap in the image area. Arm only
        // after the opening (held) touch has lifted, so it doesn't
        // self-close.
        if ctx.touches.is_empty() {
            self.armed = true;
        }
        let in_image =
            self.armed && ctx.touches.iter().any(|t| t.y > BAR_H && t.y < img_bottom);
        let tapped = self.close_pressed_last && !in_image && ctx.touches.is_empty();
        self.close_pressed_last = in_image;
        if tapped
            || ctx.pad.just_pressed(buttons::CIRCLE)
            || ctx.pad.just_pressed(buttons::CROSS)
        {
            return ScreenAction::Pop;
        }
        ScreenAction::None
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        if let WorkResponse::Image { url, .. } = resp {
            if self.inflight.as_deref() == Some(url.as_str()) {
                self.inflight = None;
            }
        }
    }
}

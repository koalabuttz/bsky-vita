//! Centralized control-hints bar.
//!
//! A one-line strip of `"<button> <action>"` segments showing a screen's
//! non-obvious controls. It auto-shows when you enter a screen and fades
//! after ~3 s (the same auto-hide idiom the video player's transport bar
//! uses), and SELECT toggles it back. main.rs owns one [`HintOverlay`],
//! ticks it each frame, and draws it on top of the active screen using
//! that screen's [`crate::screen::Screen::control_hints`].

use bsky_render::{theme, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};

use crate::tabbar::TAB_BAR_HEIGHT;

/// Frames the bar stays visible after being shown (60 fps × ~3 s).
const HIDE_FRAMES: u32 = 180;
/// Bar height in pixels (one line at scale 0.8 + padding).
const HINT_BAR_H: i32 = 26;

/// Tracks how long since the hint bar was last shown. Visible while
/// `idle < HIDE_FRAMES`.
pub struct HintOverlay {
    idle: u32,
}

impl Default for HintOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl HintOverlay {
    /// Start visible (idle = 0) so the bar greets the first screen.
    pub fn new() -> Self {
        Self { idle: 0 }
    }

    /// Show (reset the fade timer). Call on screen changes so the bar
    /// re-appears on each new page.
    pub fn show(&mut self) {
        self.idle = 0;
    }

    /// SELECT handler: hide if currently visible, else show.
    pub fn toggle(&mut self) {
        if self.visible() {
            self.idle = HIDE_FRAMES;
        } else {
            self.idle = 0;
        }
    }

    /// Advance the fade timer one frame. NOT reset by general input — only
    /// `show`/`toggle` re-show the bar, so it doesn't flash while scrolling.
    pub fn tick(&mut self) {
        self.idle = self.idle.saturating_add(1);
    }

    fn visible(&self) -> bool {
        self.idle < HIDE_FRAMES
    }

    /// Draw the bar over the current frame. No-op when hidden or when the
    /// screen supplies no hints. `has_tab_bar` (true for top-level screens)
    /// floats the bar just above the 60 px tab bar so its labels stay
    /// visible; pushed screens anchor it to the very bottom.
    pub fn draw(
        &self,
        frame: &mut Frame,
        font: &Font,
        hints: &[(&'static str, &'static str)],
        has_tab_bar: bool,
    ) {
        if !self.visible() || hints.is_empty() {
            return;
        }
        let bar_y = if has_tab_bar {
            SCREEN_HEIGHT - TAB_BAR_HEIGHT - HINT_BAR_H
        } else {
            SCREEN_HEIGHT - HINT_BAR_H
        };
        // Opaque strip + a 1px top rule (matches the pill-row divider).
        frame.fill_rect(0.0, bar_y as f32, SCREEN_WIDTH as f32, HINT_BAR_H as f32, theme::FIELD_BG);
        frame.fill_rect(0.0, bar_y as f32, SCREEN_WIDTH as f32, 1.0, theme::TEXT_MUTED);

        let scale = 0.8;
        let baseline = bar_y + HINT_BAR_H - 8;
        let mut cx = 12;
        for (i, (button, action)) in hints.iter().enumerate() {
            if i > 0 {
                cx += 8;
                frame.draw_text(font, cx, baseline, theme::TEXT_MUTED, scale, "·");
                cx += frame.measure_text(font, scale, "·").0 + 8;
            }
            // Button in accent, action in primary — quick to scan.
            frame.draw_text(font, cx, baseline, theme::ACCENT, scale, button);
            cx += frame.measure_text(font, scale, button).0 + 4;
            frame.draw_text(font, cx, baseline, theme::TEXT_PRIMARY, scale, action);
            cx += frame.measure_text(font, scale, action).0;
        }
    }
}

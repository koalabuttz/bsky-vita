//! Immediate-mode widget primitives.
//!
//! Each widget function takes `(frame, font, rect, ..., &mut state, ctx)`
//! and does both drawing and hit-testing in one pass. Persistent state
//! (button-pressed, field-value, focus) lives in the screen struct and
//! is passed in by `&mut`.
//!
//! ### Click detection
//!
//! `clicked = state.pressed_last && !pressed_now && ctx.touches.is_empty()`
//!
//! That is: a touch was inside the widget last frame, no touch is inside
//! this frame, AND there are no active touches anywhere — i.e. the user
//! actually lifted their finger. Sliding a finger out of the widget
//! without lifting does NOT fire a click.

use bsky_input::{PadFrame, TouchPoint};
use bsky_render::{theme, Color, EmojiAtlas, Font, Frame, Texture, TextureCache};
use bsky_worker::Worker;

/// Simple axis-aligned rectangle in display-pixel coordinates.
#[derive(Copy, Clone, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        let xf = x as f32;
        let yf = y as f32;
        xf >= self.x && xf < self.x + self.w && yf >= self.y && yf < self.y + self.h
    }
}

/// Per-frame input context handed to every widget.
///
/// `worker` is `None` while still in pre-auth screens (LoginScreen) and
/// `Some` for everything that runs after `ScreenAction::AuthComplete`.
/// Post-auth screens can `unwrap` it; pre-auth screens shouldn't touch it.
///
/// `emoji` is `None` if the Twemoji atlas asset isn't on the device
/// (`app0:twemoji.png` missing); screens render emoji codepoints as Inter
/// fallback (tofu) in that case. `Some` when the asset loaded
/// successfully — pass to `Frame::draw_text_*_with_emoji` to render
/// color emoji inline.
///
/// `texture_cache` is read-only access to the decoded-image LRU. Screens
/// call `texture_cache.get(&url)` to render avatars / images on cache
/// hit, and dispatch `WorkRequest::FetchImage { url }` via the worker
/// on miss. Mutations (insert on response, evict on overflow) happen in
/// `main.rs` after the worker drain.
pub struct UiCtx<'a> {
    pub touches: &'a [TouchPoint],
    pub pad: &'a PadFrame,
    pub worker: Option<&'a Worker>,
    pub emoji: Option<&'a EmojiAtlas>,
    pub texture_cache: &'a TextureCache,
    /// Pre-baked 96×96 mask: opaque background-color in the four
    /// corners, transparent disk in the center. Composited on top of
    /// rendered avatars (texture or placeholder) to fake circular
    /// avatars without GXM-direct shader work. `None` if the asset is
    /// missing — avatars then render as squares.
    pub avatar_mask: Option<&'a Texture>,
}

#[derive(Default)]
pub struct ButtonState {
    pub pressed_last: bool,
}

#[derive(Default)]
pub struct FieldState {
    pub value: String,
    pub focused: bool,
    pub pressed_last: bool,
}

/// Static text. No interaction, no state. Equivalent to a single
/// `frame.draw_text` but exposed here for parity.
pub fn label(
    frame: &mut Frame,
    font: &Font,
    x: i32,
    y: i32,
    color: Color,
    scale: f32,
    text: &str,
) {
    frame.draw_text(font, x, y, color, scale, text);
}

/// A solid-color button with centered label. Returns `true` on a clean
/// click (down-inside, up-with-no-touches). When `enabled` is false,
/// hit-test still updates state but the click event is suppressed —
/// useful for "disable interaction while modal IME is up."
pub fn button(
    frame: &mut Frame,
    font: &Font,
    rect: Rect,
    label_text: &str,
    state: &mut ButtonState,
    ctx: &UiCtx,
    enabled: bool,
) -> bool {
    let pressed_now = ctx.touches.iter().any(|t| rect.contains(t.x, t.y));
    let clicked =
        enabled && state.pressed_last && !pressed_now && ctx.touches.is_empty();
    state.pressed_last = pressed_now;

    let bg = if pressed_now {
        // Slightly darkened accent for press feedback.
        Color::rgb(0x0E, 0x6F, 0xCC)
    } else {
        theme::ACCENT
    };
    frame.fill_rect(rect.x, rect.y, rect.w, rect.h, bg);

    let scale = 1.1;
    let (tw, th) = frame.measure_text(font, scale, label_text);
    let tx = rect.x as i32 + (rect.w as i32 - tw) / 2;
    // PGF baseline; a small empirical offset puts the visual middle in
    // the rect's vertical center.
    let ty = rect.y as i32 + (rect.h as i32 + th) / 2 - 4;
    frame.draw_text(font, tx, ty, theme::TEXT_PRIMARY, scale, label_text);

    clicked
}

/// A text input field — label drawn above, then a colored rect with
/// either the current value or a placeholder. Tapping returns `true`;
/// the screen is responsible for opening the IME and stuffing the
/// result back into `state.value`. Visual focus is driven by
/// `state.focused`, which the screen toggles around the IME interaction.
///
/// `mask = true` renders one '•' per byte of `state.value` instead of
/// the value itself (used for app-password fields).
pub fn text_field(
    frame: &mut Frame,
    font: &Font,
    rect: Rect,
    label_text: &str,
    placeholder: &str,
    state: &mut FieldState,
    ctx: &UiCtx,
    mask: bool,
    enabled: bool,
) -> bool {
    let pressed_now = ctx.touches.iter().any(|t| rect.contains(t.x, t.y));
    let clicked =
        enabled && state.pressed_last && !pressed_now && ctx.touches.is_empty();
    state.pressed_last = pressed_now;

    // Label, drawn above the rect.
    let label_scale = 0.9;
    let label_y = rect.y as i32 - 8;
    frame.draw_text(font, rect.x as i32, label_y, theme::TEXT_MUTED, label_scale, label_text);

    // Field background.
    let bg = if state.focused {
        theme::FIELD_BG_FOCUS
    } else {
        theme::FIELD_BG
    };
    frame.fill_rect(rect.x, rect.y, rect.w, rect.h, bg);

    // Choose rendered text: value (possibly masked), or placeholder.
    let (display, color) = if state.value.is_empty() {
        (placeholder.to_string(), theme::TEXT_MUTED)
    } else if mask {
        ("•".repeat(state.value.len()), theme::TEXT_PRIMARY)
    } else {
        (state.value.clone(), theme::TEXT_PRIMARY)
    };

    // Truncate to fit the rect (with ellipsis suffix).
    let scale = 1.0;
    let pad_x: i32 = 12;
    let max_w = rect.w as i32 - pad_x * 2;
    let truncated = truncate_to_width(frame, font, &display, scale, max_w);

    let (_tw, th) = frame.measure_text(font, scale, &truncated);
    let tx = rect.x as i32 + pad_x;
    let ty = rect.y as i32 + (rect.h as i32 + th) / 2 - 4;
    frame.draw_text(font, tx, ty, color, scale, &truncated);

    clicked
}

/// Greedy O(n) truncation: pop bytes from the end until the rendered
/// width with an ellipsis fits in `max_w`. Acceptable performance
/// because text fields show short values and this only fires when
/// truncation is actually needed.
fn truncate_to_width(
    frame: &Frame,
    font: &Font,
    text: &str,
    scale: f32,
    max_w: i32,
) -> String {
    let (full_w, _) = frame.measure_text(font, scale, text);
    if full_w <= max_w {
        return text.to_string();
    }
    let mut s = text.to_string();
    while !s.is_empty() {
        // Pop one char (boundary-safe).
        s.pop();
        let candidate = format!("{s}…");
        let (w, _) = frame.measure_text(font, scale, &candidate);
        if w <= max_w {
            return candidate;
        }
    }
    String::from("…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_contains_inclusive_top_left_exclusive_bottom_right() {
        let r = Rect::new(10.0, 20.0, 100.0, 30.0); // x: 10..110, y: 20..50
        assert!(r.contains(10, 20));
        assert!(r.contains(50, 35));
        assert!(r.contains(109, 49));
        assert!(!r.contains(110, 49));
        assert!(!r.contains(50, 50));
        assert!(!r.contains(9, 35));
        assert!(!r.contains(50, 19));
    }

    #[test]
    fn rect_contains_negative_coords() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(!r.contains(-1, 5));
        assert!(!r.contains(5, -1));
    }
}

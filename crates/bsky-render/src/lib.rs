//! Safe Rust wrapper over libvita2d.
//!
//! Phase 2.1 surface: `Render`, `Frame`, `Color`. Just enough to clear the
//! screen and swap buffers. Phase 2.2 layers in primitives + PGF text.
//!
//! ## Lifecycle
//!
//! ```ignore
//! let mut render = Render::init().expect("vita2d init");
//! render.set_clear_color(theme::BACKGROUND);
//! loop {
//!     let _frame = render.begin_frame();
//!     // draw…
//!     // _frame's Drop calls end_drawing + swap_buffers.
//! }
//! ```
//!
//! ## Single-thread, single-instance
//!
//! libvita2d holds global state (one shared GXM context). `Render` is the
//! token of "I have called `vita2d_init`"; you only get to call it once.
//! `Render` is `!Send` (NonNull marker) and we don't expose interior
//! mutability across threads. Phase 2 is single-threaded by design.
//!
//! ## Host builds
//!
//! On non-Vita targets, every method either no-ops or returns
//! `RenderError::NotOnVita`. Host tests of dependent crates can construct
//! types but not actually render — useful only for compile-time checks.

#![allow(clippy::needless_doctest_main)]

#[cfg(target_os = "vita")]
mod ffi;

#[cfg(target_os = "vita")]
use core::ffi::c_uint;
use core::marker::PhantomData;
#[cfg(target_os = "vita")]
use core::ptr::NonNull;
#[cfg(target_os = "vita")]
use std::ffi::CString;

/// Display dimensions. The Vita's framebuffer is fixed at 960×544 regardless
/// of model (OG OLED, slim LCD, or PSTV — PSTV upscales to TV via system).
pub const SCREEN_WIDTH: i32 = 960;
pub const SCREEN_HEIGHT: i32 = 544;

/// 32-bit RGBA color matching vita2d's `RGBA8` macro layout: alpha in the
/// MSB, then blue, then green, then red in the LSB. Construct via
/// [`Color::rgb`] or [`Color::rgba`] for clarity.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Color(pub u32);

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self(
            ((a as u32) << 24)
                | ((b as u32) << 16)
                | ((g as u32) << 8)
                | (r as u32),
        )
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 0xFF)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Project-wide color theme. Matches the placeholder PNG palette under
/// `app/static/sce_sys/`.
pub mod theme {
    use super::Color;
    pub const BACKGROUND: Color = Color::rgb(0x0F, 0x17, 0x2A); // dark slate
    pub const ACCENT: Color = Color::rgb(0x11, 0x85, 0xFE); // Bsky blue
    pub const TEXT_PRIMARY: Color = Color::rgb(0xF5, 0xF5, 0xF5);
    pub const TEXT_MUTED: Color = Color::rgb(0x90, 0xA0, 0xB0);
    pub const ERROR: Color = Color::rgb(0xE0, 0x4A, 0x4A);
    pub const FIELD_BG: Color = Color::rgb(0x1E, 0x29, 0x40);
    pub const FIELD_BG_FOCUS: Color = Color::rgb(0x2A, 0x36, 0x4F);
}

#[derive(Debug)]
pub enum RenderError {
    /// `vita2d_init` returned a negative status code.
    Init(i32),
    /// `vita2d_load_*_pgf` returned a null pointer.
    PgfLoad,
    /// `vita2d_load_font_file` returned a null pointer (asset missing,
    /// corrupt, or FreeType failed to parse).
    TtfLoad,
    /// Method was called on a non-Vita target where rendering is unavailable.
    NotOnVita,
}

impl core::fmt::Display for RenderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RenderError::Init(code) => write!(f, "vita2d_init failed: {code}"),
            RenderError::PgfLoad => write!(f, "vita2d_load_default_pgf returned null"),
            RenderError::TtfLoad => write!(f, "vita2d_load_font_file returned null"),
            RenderError::NotOnVita => write!(f, "render is only supported on the Vita target"),
        }
    }
}

/// Pixel size that `scale = 1.0` maps to when rendering with TTF fonts.
/// 20 was tuned on hardware to compensate for the Vita's lower DPI vs a
/// modern phone — Bluesky's mobile app uses ~15–16 px body text at 400+
/// DPI; on the Vita's ~220 DPI we need ~20 px to give each glyph enough
/// rasterized pixels per stroke for clean antialiasing. PGF rendering
/// ignores this — it uses the float `scale` directly because PGF is a
/// bitmap font with its own native size.
#[cfg(target_os = "vita")]
const BASE_SIZE_PX: u32 = 20;

/// Convert the `scale: f32` carried through the bsky-render API into the
/// `unsigned int size` that `vita2d_font_*` expects. Clamps to ≥ 1 so a
/// near-zero scale doesn't render at size 0 (which vita2d treats as
/// "draw nothing").
#[cfg(target_os = "vita")]
#[inline]
fn scale_to_px(scale: f32) -> c_uint {
    (scale * BASE_SIZE_PX as f32).round().max(1.0) as c_uint
}

impl core::error::Error for RenderError {}

/// Owns the vita2d global state. Constructed once; dropping it tears down
/// libvita2d. Not `Send` (vita2d is single-thread).
pub struct Render {
    /// Marker to make Render `!Send` and `!Sync`. (`PhantomData<*const ()>`
    /// is the canonical pattern.)
    _not_send: PhantomData<*const ()>,
}

impl Render {
    /// Initialize libvita2d. Call exactly once at app startup.
    pub fn init() -> Result<Self, RenderError> {
        #[cfg(target_os = "vita")]
        {
            let r = unsafe { ffi::vita2d_init() };
            if r < 0 {
                return Err(RenderError::Init(r));
            }
            Ok(Self { _not_send: PhantomData })
        }
        #[cfg(not(target_os = "vita"))]
        {
            Err(RenderError::NotOnVita)
        }
    }

    /// Set the color used by [`Frame`]'s implicit clear. Persists across
    /// frames; call once at startup with the theme background.
    pub fn set_clear_color(&mut self, color: Color) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_set_clear_color(color.raw());
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = color;
        }
    }

    /// Enable or disable waiting for vertical blank during `swap_buffers`.
    /// Default is enabled (60 fps cap, smooth presentation). Disabling
    /// burns CPU; only useful for benchmarking.
    pub fn set_vblank_wait(&mut self, enable: bool) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_set_vblank_wait(enable as i32);
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = enable;
        }
    }

    /// Begin a new frame. Calls `vita2d_start_drawing` + `vita2d_clear_screen`.
    /// The returned [`Frame`] commits and presents on drop.
    pub fn begin_frame(&mut self) -> Frame<'_> {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_start_drawing();
            ffi::vita2d_clear_screen();
        }
        Frame {
            ended: false,
            _render: PhantomData,
        }
    }

    /// Load Sony's default system PGF font (Japanese; also covers Latin
    /// glyphs used by Bluesky handles + display names well enough for
    /// Phase 2). Call once at startup; the resulting [`Font`] is reused
    /// across all frames.
    ///
    /// PGF symbols are weak imports in libvita2d.h; `libvita2d_ext.a`
    /// resolves them at link time and the actual implementation comes
    /// from `libScePgf_stub.a` (already linked via `vitasdk-sys` +
    /// `bsky-render`'s build.rs).
    pub fn load_default_pgf(&self) -> Result<Font, RenderError> {
        #[cfg(target_os = "vita")]
        {
            let p = unsafe { ffi::vita2d_load_default_pgf() };
            match NonNull::new(p) {
                Some(ptr) => Ok(Font::Pgf(ptr)),
                None => Err(RenderError::PgfLoad),
            }
        }
        #[cfg(not(target_os = "vita"))]
        {
            Err(RenderError::NotOnVita)
        }
    }

    /// Load a TrueType / OpenType font from a Vita filesystem path
    /// (typically `app0:Inter-Regular.ttf` for a bundled VPK asset).
    /// Backed by FreeType inside vita2d. Phase 3.3+ default; PGF stays
    /// available via [`Render::load_default_pgf`] as a fallback if the
    /// asset is missing or corrupt.
    ///
    /// The returned [`Font`] uses the same `scale: f32` API as PGF — the
    /// scale is multiplied by [`BASE_SIZE_PX`] internally to derive the
    /// pixel size FreeType needs.
    pub fn load_inter_ttf(&self, path: &str) -> Result<Font, RenderError> {
        #[cfg(target_os = "vita")]
        {
            // vita2d's `vita2d_load_font_file` crashes (instead of
            // returning NULL) when the file is missing — confirmed on
            // hardware. Pre-check existence via std::fs so the fallback
            // path stays safe.
            if std::fs::metadata(path).is_err() {
                return Err(RenderError::TtfLoad);
            }
            let cstr = CString::new(path).map_err(|_| RenderError::TtfLoad)?;
            let p = unsafe { ffi::vita2d_load_font_file(cstr.as_ptr()) };
            match NonNull::new(p) {
                Some(ptr) => Ok(Font::Ttf(ptr)),
                None => Err(RenderError::TtfLoad),
            }
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = path;
            Err(RenderError::NotOnVita)
        }
    }
}

/// A loaded font handle. Single-threaded; freed via the matching
/// `vita2d_free_*` on Drop. Two backends:
///
/// - [`Font::Pgf`] — Sony's bitmap font. Renders at fixed sizes; the
///   `scale` parameter is a float multiplier on PGF's native size.
///   Always available without bundled assets. Used as the fallback when
///   the TTF asset is missing.
/// - [`Font::Ttf`] — FreeType-rendered TrueType / OpenType. The `scale`
///   parameter is multiplied by [`BASE_SIZE_PX`] inside the wrapper to
///   produce the pixel size FreeType needs. Higher quality, supports
///   any size cleanly, but requires a bundled font asset.
pub enum Font {
    #[cfg(target_os = "vita")]
    Pgf(NonNull<ffi::vita2d_pgf>),
    #[cfg(target_os = "vita")]
    Ttf(NonNull<ffi::vita2d_font>),
    #[cfg(not(target_os = "vita"))]
    Stub(PhantomData<*const ()>),
}

impl Drop for Font {
    fn drop(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            match self {
                Font::Pgf(p) => ffi::vita2d_free_pgf(p.as_ptr()),
                Font::Ttf(p) => ffi::vita2d_free_font(p.as_ptr()),
            }
        }
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_wait_rendering_done();
            ffi::vita2d_fini();
        }
    }
}

/// A single drawing frame. Holds an exclusive borrow on `Render` so two
/// frames can't overlap.
///
/// Drop semantics ensure the correct vita2d frame ordering even when an
/// optional [`pump_ime`](Frame::pump_ime) call is in the mix:
///
/// ```text
///   begin_frame  →  start_drawing + clear_screen
///   …draws…
///   pump_ime?    →  end_drawing  +  vita2d_common_dialog_update
///   Drop         →  end_drawing (if pump_ime wasn't called)
///                +  swap_buffers
/// ```
///
/// `vita2d_common_dialog_update` MUST land after `vita2d_end_drawing` and
/// before `vita2d_swap_buffers` for modal dialogs (sceImeDialog,
/// sceMsgDialog, etc.) to actually paint onto the back buffer. Calling it
/// in the wrong slot leaves the dialog active-but-invisible, with input
/// captured by an unseen keyboard.
pub struct Frame<'r> {
    /// Have we already called `vita2d_end_drawing` (via `pump_ime`)?
    /// Drop checks this to avoid a double-end. Only used on the Vita
    /// target; host stub Frames don't actually render.
    #[cfg_attr(not(target_os = "vita"), allow(dead_code))]
    ended: bool,
    _render: PhantomData<&'r mut Render>,
}

impl<'r> Frame<'r> {
    /// Filled rectangle. Coordinates are display pixels (0..960 × 0..544).
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_draw_rectangle(x, y, w, h, color.raw());
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (x, y, w, h, color);
        }
    }

    /// Single-pixel-wide line.
    pub fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, color: Color) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_draw_line(x0, y0, x1, y1, color.raw());
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (x0, y0, x1, y1, color);
        }
    }

    /// Draw `text` at integer pixel position `(x, y)` (the y is the text
    /// baseline). Returns the x-position immediately after the last glyph,
    /// suitable for chaining sequential `draw_text` calls.
    ///
    /// Any embedded NUL bytes are stripped before passing to vita2d. UTF-8
    /// strings render as PGF glyphs; characters outside the loaded language
    /// pack render as the system tofu glyph.
    pub fn draw_text(
        &mut self,
        font: &Font,
        x: i32,
        y: i32,
        color: Color,
        scale: f32,
        text: &str,
    ) -> i32 {
        #[cfg(target_os = "vita")]
        {
            let cstr = match CString::new(text.replace('\0', "")) {
                Ok(s) => s,
                Err(_) => return x,
            };
            match font {
                Font::Pgf(p) => unsafe {
                    ffi::vita2d_pgf_draw_text(
                        p.as_ptr(),
                        x,
                        y,
                        color.raw(),
                        scale,
                        cstr.as_ptr(),
                    )
                },
                Font::Ttf(p) => unsafe {
                    ffi::vita2d_font_draw_text(
                        p.as_ptr(),
                        x,
                        y,
                        color.raw(),
                        scale_to_px(scale),
                        cstr.as_ptr(),
                    )
                },
            }
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (font, y, color, scale, text);
            x
        }
    }

    /// Measure `text` as it would render with the given font + scale.
    /// Returns `(width, height)` in display pixels.
    pub fn measure_text(&self, font: &Font, scale: f32, text: &str) -> (i32, i32) {
        #[cfg(target_os = "vita")]
        {
            let cstr = match CString::new(text.replace('\0', "")) {
                Ok(s) => s,
                Err(_) => return (0, 0),
            };
            let mut w: i32 = 0;
            let mut h: i32 = 0;
            match font {
                Font::Pgf(p) => unsafe {
                    ffi::vita2d_pgf_text_dimensions(
                        p.as_ptr(),
                        scale,
                        cstr.as_ptr(),
                        &mut w,
                        &mut h,
                    );
                },
                Font::Ttf(p) => unsafe {
                    ffi::vita2d_font_text_dimensions(
                        p.as_ptr(),
                        scale_to_px(scale),
                        cstr.as_ptr(),
                        &mut w,
                        &mut h,
                    );
                },
            }
            (w, h)
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (font, scale, text);
            (0, 0)
        }
    }

    /// Convenience: draw `text` centered horizontally on the screen at
    /// the given y baseline. Returns the bounding box (x, y, w, h) so
    /// callers can stack labels.
    pub fn draw_text_centered(
        &mut self,
        font: &Font,
        y: i32,
        color: Color,
        scale: f32,
        text: &str,
    ) -> (i32, i32, i32, i32) {
        let (w, h) = self.measure_text(font, scale, text);
        let x = (SCREEN_WIDTH - w) / 2;
        self.draw_text(font, x, y, color, scale, text);
        (x, y, w, h)
    }

    /// Word-wrap `text` to fit within `max_w` pixels at `scale`, drawing
    /// each line at increasing y-offsets from `y`. Returns the total
    /// drawn height (`n_lines * line_h`); pass to layout code to advance
    /// the cursor past the wrapped block.
    ///
    /// Wrap policy: split on whitespace; words that exceed `max_w` are
    /// hard-broken on `char` boundaries (no shaping or hyphenation).
    /// Embedded `\n` characters force a paragraph break. Empty input
    /// returns 0 without drawing.
    ///
    /// Performance: the implementation calls `measure_text` once per
    /// candidate line plus once per oversized-word character. For 3.2's
    /// timeline post bodies (~5 visible posts × 50ish chars) this is a
    /// few hundred FFI calls per frame — comfortably below 60fps budget
    /// on Cortex-A9. Use [`Frame::measure_text_wrapped`] to compute
    /// heights in advance (e.g. for layout culling) without paying the
    /// glyph rasterization cost.
    pub fn draw_text_wrapped(
        &mut self,
        font: &Font,
        x: i32,
        y: i32,
        max_w: i32,
        color: Color,
        scale: f32,
        text: &str,
    ) -> i32 {
        if text.is_empty() {
            return 0;
        }
        let (_, ref_h) = self.measure_text(font, scale, "Hg");
        let line_h = ref_h + 4;
        let mut y_cursor = y;
        for paragraph in text.split('\n') {
            let mut current = String::new();
            for word in paragraph.split_whitespace() {
                let candidate = if current.is_empty() {
                    word.to_string()
                } else {
                    format!("{current} {word}")
                };
                if self.measure_text(font, scale, &candidate).0 <= max_w {
                    current = candidate;
                    continue;
                }
                if !current.is_empty() {
                    self.draw_text(font, x, y_cursor, color, scale, &current);
                    y_cursor += line_h;
                    current.clear();
                }
                if self.measure_text(font, scale, word).0 <= max_w {
                    current = word.to_string();
                } else {
                    for ch in word.chars() {
                        let trial = format!("{current}{ch}");
                        if self.measure_text(font, scale, &trial).0 > max_w
                            && !current.is_empty()
                        {
                            self.draw_text(font, x, y_cursor, color, scale, &current);
                            y_cursor += line_h;
                            current.clear();
                        }
                        current.push(ch);
                    }
                }
            }
            if !current.is_empty() {
                self.draw_text(font, x, y_cursor, color, scale, &current);
                y_cursor += line_h;
            }
        }
        y_cursor - y
    }

    /// Measure how tall `text` would be when word-wrapped to `max_w`
    /// pixels at `scale`, without drawing anything. Returns the height
    /// `draw_text_wrapped` would advance by. Useful for pre-layout
    /// culling (e.g. computing post-row heights before deciding which
    /// rows are visible). Mirrors `draw_text_wrapped`'s wrap policy
    /// exactly; if you change one, change both.
    pub fn measure_text_wrapped(
        &self,
        font: &Font,
        max_w: i32,
        scale: f32,
        text: &str,
    ) -> i32 {
        if text.is_empty() {
            return 0;
        }
        let (_, ref_h) = self.measure_text(font, scale, "Hg");
        let line_h = ref_h + 4;
        let mut y_cursor = 0;
        for paragraph in text.split('\n') {
            let mut current = String::new();
            for word in paragraph.split_whitespace() {
                let candidate = if current.is_empty() {
                    word.to_string()
                } else {
                    format!("{current} {word}")
                };
                if self.measure_text(font, scale, &candidate).0 <= max_w {
                    current = candidate;
                    continue;
                }
                if !current.is_empty() {
                    y_cursor += line_h;
                    current.clear();
                }
                if self.measure_text(font, scale, word).0 <= max_w {
                    current = word.to_string();
                } else {
                    for ch in word.chars() {
                        let trial = format!("{current}{ch}");
                        if self.measure_text(font, scale, &trial).0 > max_w
                            && !current.is_empty()
                        {
                            y_cursor += line_h;
                            current.clear();
                        }
                        current.push(ch);
                    }
                }
            }
            if !current.is_empty() {
                y_cursor += line_h;
            }
        }
        y_cursor
    }

    /// Drive a modal common dialog (e.g. sceImeDialog) for one frame.
    ///
    /// This ends the GXM scene first (so any draws above are committed),
    /// then calls `vita2d_common_dialog_update` to overlay the dialog
    /// onto the back buffer. The buffer swap still happens on Drop, so
    /// the resulting per-frame sequence is `end_drawing → dialog_update →
    /// swap_buffers` — which is what the system expects for modal dialogs
    /// to actually render.
    ///
    /// Calling `pump_ime` more than once per frame is fine but redundant;
    /// only the first call ends the scene, subsequent calls just paint
    /// the (already-ended) frame again.
    pub fn pump_ime(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            if !self.ended {
                ffi::vita2d_end_drawing();
                self.ended = true;
            }
            ffi::vita2d_common_dialog_update();
        }
    }
}

impl<'r> Drop for Frame<'r> {
    fn drop(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            if !self.ended {
                ffi::vita2d_end_drawing();
            }
            ffi::vita2d_swap_buffers();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_packing_matches_vita2d_RGBA8_macro() {
        // RGBA8(r,g,b,a) = (a<<24) | (b<<16) | (g<<8) | r — host check.
        let c = Color::rgba(0x12, 0x34, 0x56, 0x78);
        assert_eq!(c.raw(), 0x78_56_34_12);
    }

    #[test]
    fn rgb_sets_alpha_to_full() {
        let c = Color::rgb(0x11, 0x22, 0x33);
        assert_eq!(c.raw(), 0xFF_33_22_11);
    }

    #[test]
    fn theme_constants_have_full_alpha() {
        for c in [
            theme::BACKGROUND,
            theme::ACCENT,
            theme::TEXT_PRIMARY,
            theme::TEXT_MUTED,
            theme::ERROR,
            theme::FIELD_BG,
            theme::FIELD_BG_FOCUS,
        ] {
            assert_eq!(c.raw() >> 24, 0xFF, "{c:?} should have full alpha");
        }
    }

    #[cfg(not(target_os = "vita"))]
    #[test]
    fn host_init_returns_not_on_vita() {
        match Render::init() {
            Err(RenderError::NotOnVita) => {}
            Err(other) => panic!("expected NotOnVita on host, got {other:?}"),
            Ok(_) => panic!("expected NotOnVita on host, got Ok"),
        }
    }
}

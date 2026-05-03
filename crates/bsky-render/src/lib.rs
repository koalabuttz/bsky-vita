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

use core::marker::PhantomData;

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
    /// Method was called on a non-Vita target where rendering is unavailable.
    NotOnVita,
}

impl core::fmt::Display for RenderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RenderError::Init(code) => write!(f, "vita2d_init failed: {code}"),
            RenderError::NotOnVita => write!(f, "render is only supported on the Vita target"),
        }
    }
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
        Frame { _render: PhantomData }
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
/// frames can't overlap. Drop calls `vita2d_end_drawing` + `vita2d_swap_buffers`.
pub struct Frame<'r> {
    _render: PhantomData<&'r mut Render>,
}

impl<'r> Frame<'r> {
    /// Drive a modal common dialog (e.g. sceImeDialog) for one frame. Must
    /// be called *between* draw calls and the implicit swap-on-drop —
    /// that's automatic since `pump_ime` takes `&mut self`.
    ///
    /// Phase 2.3 turns this into a real path; for Phase 2.1 it exists as
    /// a no-op so the API surface is stable.
    pub fn pump_ime(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_common_dialog_update();
        }
    }
}

impl<'r> Drop for Frame<'r> {
    fn drop(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_end_drawing();
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

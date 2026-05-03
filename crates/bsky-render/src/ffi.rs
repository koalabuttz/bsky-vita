//! Hand-rolled `extern "C"` declarations for libvita2d.
//!
//! Source: `$VITASDK/arm-vita-eabi/include/vita2d.h` (191 lines).
//!
//! Phase 2.1 only declares the lifecycle + clear/swap primitives we need
//! for "open vita2d, clear to a color, swap." Phase 2.2 will add primitives
//! and PGF text; Phase 3 will add textures and image loading.
//!
//! Everything in this module is gated `#[cfg(target_os = "vita")]` so host
//! checks of bsky-render don't try to link vita2d. Consumers of bsky-render
//! never reach into this module directly — the safe wrappers in `lib.rs`
//! do, with the same gate.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use core::ffi::{c_char, c_float, c_int, c_uint};

// `vita2d.h` declares `vita2d_pgf`, `vita2d_pvf`, `vita2d_font`, and
// `vita2d_texture` as forward-declared structs we never inspect directly.
// We model them as opaque types (uninhabited enums) — a common Rust idiom
// for opaque C handles. `*mut vita2d_pgf` is what we pass around.
pub enum vita2d_pgf {}
pub enum vita2d_pvf {}
pub enum vita2d_font {}
pub enum vita2d_texture {}

unsafe extern "C" {
    // Lifecycle
    pub fn vita2d_init() -> c_int;
    pub fn vita2d_fini() -> c_int;
    pub fn vita2d_wait_rendering_done();

    // Frame
    pub fn vita2d_start_drawing();
    pub fn vita2d_end_drawing();
    pub fn vita2d_swap_buffers();
    pub fn vita2d_clear_screen();

    // Clear color + display
    pub fn vita2d_set_clear_color(color: c_uint);
    pub fn vita2d_get_clear_color() -> c_uint;
    pub fn vita2d_set_vblank_wait(enable: c_int);

    // Common dialog (IME) integration — wraps sceCommonDialogUpdate with
    // the current vita2d framebuffer info. Must be called between
    // start_drawing and end_drawing each frame while a modal dialog is up.
    pub fn vita2d_common_dialog_update() -> c_int;

    // Primitive drawing
    pub fn vita2d_draw_pixel(x: c_float, y: c_float, color: c_uint);
    pub fn vita2d_draw_line(x0: c_float, y0: c_float, x1: c_float, y1: c_float, color: c_uint);
    pub fn vita2d_draw_rectangle(
        x: c_float,
        y: c_float,
        w: c_float,
        h: c_float,
        color: c_uint,
    );

    // PGF system font (Sony's bitmap font; covers Latin + Japanese + Chinese
    // + Korean depending on which language packs are loaded).
    pub fn vita2d_load_default_pgf() -> *mut vita2d_pgf;
    pub fn vita2d_load_custom_pgf(path: *const c_char) -> *mut vita2d_pgf;
    pub fn vita2d_free_pgf(font: *mut vita2d_pgf);
    pub fn vita2d_pgf_draw_text(
        font: *mut vita2d_pgf,
        x: c_int,
        y: c_int,
        color: c_uint,
        scale: c_float,
        text: *const c_char,
    ) -> c_int;
    pub fn vita2d_pgf_text_width(
        font: *mut vita2d_pgf,
        scale: c_float,
        text: *const c_char,
    ) -> c_int;
    pub fn vita2d_pgf_text_height(
        font: *mut vita2d_pgf,
        scale: c_float,
        text: *const c_char,
    ) -> c_int;
    pub fn vita2d_pgf_text_dimensions(
        font: *mut vita2d_pgf,
        scale: c_float,
        text: *const c_char,
        width: *mut c_int,
        height: *mut c_int,
    );
}

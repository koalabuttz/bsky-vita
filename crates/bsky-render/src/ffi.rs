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

use core::ffi::{c_char, c_float, c_int, c_uint, c_ulong, c_void};

// `vita2d.h` declares `vita2d_pgf`, `vita2d_pvf`, `vita2d_font` as
// forward-declared structs we never inspect directly. Opaque enums
// suffice for those — `*mut vita2d_pgf` is what we pass around.
pub enum vita2d_pgf {}
pub enum vita2d_pvf {}
pub enum vita2d_font {}

// `vita2d_texture` we DO inspect — Phase 5.3.x builds a YUV-format
// texture by hand-constructing this struct (sceGxmTextureInitLinear
// with the YUV format the GPU sampler natively understands, so the
// hardware does YUV→RGB conversion at sample time). vita2d's
// `vita2d_create_empty_texture_format` mishandles YUV strides, so
// we bypass it. Layout mirrors `$VITASDK/arm-vita-eabi/include/
// vita2d.h:36-44` exactly:
//
//   typedef struct vita2d_texture {
//       SceGxmTexture          gxm_tex;
//       SceUID                 data_UID;
//       SceUID                 palette_UID;
//       SceGxmRenderTarget    *gxm_rtgt;
//       SceGxmColorSurface     gxm_sfc;
//       SceGxmDepthStencilSurface gxm_sfd;
//       SceUID                 depth_UID;
//   } vita2d_texture;
//
// Sizes for the Sce* structs come from vitasdk-sys to guarantee the
// ABI matches what libvita2d was compiled against.
#[repr(C)]
pub struct vita2d_texture {
    pub gxm_tex: vitasdk_sys::SceGxmTexture,
    pub data_uid: vitasdk_sys::SceUID,
    pub palette_uid: vitasdk_sys::SceUID,
    pub gxm_rtgt: *mut c_void,
    pub gxm_sfc: vitasdk_sys::SceGxmColorSurface,
    pub gxm_sfd: vitasdk_sys::SceGxmDepthStencilSurface,
    pub depth_uid: vitasdk_sys::SceUID,
}

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
    pub fn vita2d_draw_fill_circle(
        x: c_float,
        y: c_float,
        radius: c_float,
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

    // FreeType-backed TrueType / OpenType font (Phase 3.3). Same
    // call-shape as PGF except `size` is unsigned pixel size (not float
    // scale), and the loader takes a path or memory blob instead of
    // pulling a built-in system font.
    pub fn vita2d_load_font_file(filename: *const c_char) -> *mut vita2d_font;
    pub fn vita2d_free_font(font: *mut vita2d_font);
    pub fn vita2d_font_draw_text(
        font: *mut vita2d_font,
        x: c_int,
        y: c_int,
        color: c_uint,
        size: c_uint,
        text: *const c_char,
    ) -> c_int;
    pub fn vita2d_font_text_dimensions(
        font: *mut vita2d_font,
        size: c_uint,
        text: *const c_char,
        width: *mut c_int,
        height: *mut c_int,
    );

    // Texture lifecycle + image loaders (Phase 3.4). PNG buffer takes no
    // size — vita2d's PNG path reads the file-length header from the PNG
    // itself; JPEG takes an explicit size since JPEG has no length header.
    pub fn vita2d_load_PNG_buffer(buffer: *const c_void) -> *mut vita2d_texture;
    pub fn vita2d_load_PNG_file(filename: *const c_char) -> *mut vita2d_texture;
    pub fn vita2d_load_JPEG_buffer(
        buffer: *const c_void,
        buffer_size: c_ulong,
    ) -> *mut vita2d_texture;
    pub fn vita2d_free_texture(texture: *mut vita2d_texture);

    pub fn vita2d_texture_get_width(texture: *const vita2d_texture) -> c_uint;
    pub fn vita2d_texture_get_height(texture: *const vita2d_texture) -> c_uint;

    // Empty-texture creation for video frames (Phase 5.3). The
    // `format` arg takes a `SceGxmTextureFormat` numeric constant —
    // for video we use `SCE_GXM_TEXTURE_FORMAT_YUV420P3_CSC0`
    // (= 0x90F00000), defined in `crate::Texture`.
    pub fn vita2d_create_empty_texture_format(
        w: c_uint,
        h: c_uint,
        format: c_uint,
    ) -> *mut vita2d_texture;
    /// Pointer to the texture's CPU-mapped buffer. For YUV420P3 the
    /// buffer holds Y / U / V planes back-to-back, each plane stride =
    /// `vita2d_texture_get_stride` (texture stride is uniform; chroma
    /// planes are simply half-pitch from libvita2d's perspective).
    pub fn vita2d_texture_get_datap(texture: *const vita2d_texture) -> *mut c_void;
    pub fn vita2d_texture_get_stride(texture: *const vita2d_texture) -> c_uint;

    // Draw variants. The `_part_scale` form picks a sub-rectangle from
    // the source texture (atlas) and scales it independently per axis —
    // exactly what we need for emoji glyph rendering.
    pub fn vita2d_draw_texture(
        texture: *const vita2d_texture,
        x: c_float,
        y: c_float,
    );
    pub fn vita2d_draw_texture_scale(
        texture: *const vita2d_texture,
        x: c_float,
        y: c_float,
        x_scale: c_float,
        y_scale: c_float,
    );
    pub fn vita2d_draw_texture_part_scale(
        texture: *const vita2d_texture,
        x: c_float,
        y: c_float,
        tex_x: c_float,
        tex_y: c_float,
        tex_w: c_float,
        tex_h: c_float,
        x_scale: c_float,
        y_scale: c_float,
    );

    // Phase 5.3.x.1 — vita2d's GXM context + shader patcher accessors.
    // We register our YUV→RGB shader programs with vita2d's existing
    // patcher and submit our draw to vita2d's existing context, rather
    // than standing up a parallel GXM init dance. The pool helpers give
    // us GPU-mapped per-frame scratch (vertex buffer for the video
    // quad); the linear-indices accessor returns a pre-allocated
    // GPU-mapped index buffer of [0, 1, 2, …, MAX].
    pub fn vita2d_get_context() -> *mut vitasdk_sys::SceGxmContext;
    pub fn vita2d_get_shader_patcher() -> *mut vitasdk_sys::SceGxmShaderPatcher;
    pub fn vita2d_get_linear_indices() -> *const u16;
    pub fn vita2d_pool_memalign(size: c_uint, alignment: c_uint) -> *mut c_void;
}

// Newlib's malloc/free, linked via libc (already on the link line for
// every Vita binary). SHACCCG's `sceShaccCgSetDefaultAllocator` needs
// these as callbacks so its internal scratch buffers can be allocated.
unsafe extern "C" {
    pub fn malloc(size: c_uint) -> *mut c_void;
    pub fn free(ptr: *mut c_void);
}

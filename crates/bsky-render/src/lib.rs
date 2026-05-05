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
// ATLAS_COLS / ATLAS_ROWS are used by external tooling readers; keep
// them in the generated file for documentation even if unread by us.
#[allow(dead_code)]
mod emoji_table;

#[cfg(target_os = "vita")]
use core::ffi::c_uint;
use core::marker::PhantomData;
#[cfg(target_os = "vita")]
use core::ptr::NonNull;
use std::collections::{HashMap, VecDeque};
#[cfg(target_os = "vita")]
use std::ffi::CString;

/// Display dimensions. The Vita's framebuffer is fixed at 960×544 regardless
/// of model (OG OLED, slim LCD, or PSTV — PSTV upscales to TV via system).
pub const SCREEN_WIDTH: i32 = 960;
pub const SCREEN_HEIGHT: i32 = 544;


/// `SceGxmTextureFormat::SCE_GXM_TEXTURE_FORMAT_A8B8G8R8` — standard
/// RGBA texture used by `Texture::create_yuv420`.
#[cfg(target_os = "vita")]
const SCE_GXM_TEXTURE_FORMAT_A8B8G8R8: c_uint = 0x0C00_0000;

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
    /// `vita2d_load_PNG_*` or `vita2d_load_JPEG_*` returned a null
    /// pointer (asset missing, corrupt, or unsupported format).
    /// The `&'static str` carries which loader failed.
    TextureLoad(&'static str),
    /// Method was called on a non-Vita target where rendering is unavailable.
    NotOnVita,
}

impl core::fmt::Display for RenderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RenderError::Init(code) => write!(f, "vita2d_init failed: {code}"),
            RenderError::PgfLoad => write!(f, "vita2d_load_default_pgf returned null"),
            RenderError::TtfLoad => write!(f, "vita2d_load_font_file returned null"),
            RenderError::TextureLoad(what) => write!(f, "texture load failed ({what})"),
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

/// A loaded GPU texture handle (vita2d-backed). Created from PNG or JPEG
/// bytes (in-memory) or a PNG file. Single-threaded; freed via
/// `vita2d_free_texture` on Drop. Held by [`EmojiAtlas`] and
/// [`TextureCache`].
pub struct Texture {
    #[cfg(target_os = "vita")]
    ptr: NonNull<ffi::vita2d_texture>,
    width: i32,
    height: i32,
}

impl Texture {
    /// Decode `bytes` as PNG. Returns [`RenderError::TextureLoad`] if the
    /// data isn't a valid PNG (or vita2d's PNG path otherwise rejects it).
    pub fn from_png_bytes(bytes: &[u8]) -> Result<Self, RenderError> {
        // PNG magic: 0x89 'P' 'N' 'G' 0x0D 0x0A 0x1A 0x0A
        if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
            return Err(RenderError::TextureLoad("PNG: bad magic"));
        }
        #[cfg(target_os = "vita")]
        {
            let p = unsafe { ffi::vita2d_load_PNG_buffer(bytes.as_ptr() as *const _) };
            Self::wrap_raw(p, "PNG")
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = bytes;
            Err(RenderError::NotOnVita)
        }
    }

    /// Decode `bytes` as JPEG.
    pub fn from_jpeg_bytes(bytes: &[u8]) -> Result<Self, RenderError> {
        // JPEG magic: 0xFF 0xD8 0xFF
        if bytes.len() < 3 || bytes[0] != 0xFF || bytes[1] != 0xD8 || bytes[2] != 0xFF {
            return Err(RenderError::TextureLoad("JPEG: bad magic"));
        }
        #[cfg(target_os = "vita")]
        {
            let p = unsafe {
                ffi::vita2d_load_JPEG_buffer(bytes.as_ptr() as *const _, bytes.len() as _)
            };
            Self::wrap_raw(p, "JPEG")
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = bytes;
            Err(RenderError::NotOnVita)
        }
    }

    /// Auto-detect PNG vs JPEG from magic bytes and dispatch.
    pub fn from_image_bytes(bytes: &[u8]) -> Result<Self, RenderError> {
        if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
            Self::from_png_bytes(bytes)
        } else if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
            Self::from_jpeg_bytes(bytes)
        } else {
            Err(RenderError::TextureLoad("unknown image format"))
        }
    }

    /// Load a PNG from a Vita filesystem path (e.g. `app0:twemoji.png`).
    pub fn from_png_file(path: &str) -> Result<Self, RenderError> {
        #[cfg(target_os = "vita")]
        {
            // vita2d_load_PNG_file crashes on missing files (same bug as
            // vita2d_load_font_file). Pre-check via std::fs to keep the
            // fallback path safe.
            if std::fs::metadata(path).is_err() {
                return Err(RenderError::TextureLoad("PNG: file not found"));
            }
            let cstr = CString::new(path)
                .map_err(|_| RenderError::TextureLoad("PNG: path has interior NUL"))?;
            let p = unsafe { ffi::vita2d_load_PNG_file(cstr.as_ptr()) };
            Self::wrap_raw(p, "PNG (file)")
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = path;
            Err(RenderError::NotOnVita)
        }
    }

    pub fn width(&self) -> i32 {
        self.width
    }
    pub fn height(&self) -> i32 {
        self.height
    }

    /// Allocate a full-resolution RGBA8 texture for video frames.
    /// Color reproduction would need a custom GXM fragment shader
    /// that does YUV→RGB conversion (vita2d's stock shader binding
    /// doesn't activate the sampler's hardware CSC path even for
    /// YUV-format textures). For 5.3.x we ship greyscale — read the
    /// Y plane and spread to RGBA in [`Texture::upload_yuv420`].
    /// Color is a known follow-up.
    pub fn create_yuv420(width: u32, height: u32) -> Result<Self, RenderError> {
        #[cfg(target_os = "vita")]
        {
            let p = unsafe {
                ffi::vita2d_create_empty_texture_format(
                    width,
                    height,
                    SCE_GXM_TEXTURE_FORMAT_A8B8G8R8,
                )
            };
            Self::wrap_raw(p, "RGBA8")
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (width, height);
            Err(RenderError::NotOnVita)
        }
    }

    /// Greyscale upload: read each Y byte, splat it across R/G/B of
    /// the RGBA texture (alpha = 0xFF). Ignores U/V planes — color
    /// is deferred until a custom GXM YUV shader lands. The
    /// per-pixel cost is one read + one 4-byte packed write, no
    /// math; ~140 MB/s of memory bandwidth at 720p/30 fps which the
    /// Vita's CPU + RAM can sustain.
    pub fn upload_yuv420(
        &self,
        y: &[u8],
        y_pitch: usize,
        _u: &[u8],
        _u_pitch: usize,
        _v: &[u8],
        _v_pitch: usize,
        width: u32,
        height: u32,
    ) {
        #[cfg(target_os = "vita")]
        unsafe {
            let dst_stride = ffi::vita2d_texture_get_stride(self.ptr.as_ptr()) as usize;
            let base = ffi::vita2d_texture_get_datap(self.ptr.as_ptr()) as *mut u8;
            if base.is_null() {
                return;
            }
            let w = width as usize;
            let h = height as usize;
            for row in 0..h {
                let src = y.as_ptr().add(row * y_pitch);
                let dst = base.add(row * dst_stride) as *mut u32;
                for col in 0..w {
                    let yv = *src.add(col) as u32;
                    *dst.add(col) = 0xFF00_0000 | (yv << 16) | (yv << 8) | yv;
                }
            }
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (y, y_pitch, _u, _u_pitch, _v, _v_pitch, width, height);
        }
    }

    #[cfg(target_os = "vita")]
    fn wrap_raw(
        p: *mut ffi::vita2d_texture,
        what: &'static str,
    ) -> Result<Self, RenderError> {
        let ptr = NonNull::new(p).ok_or(RenderError::TextureLoad(what))?;
        let width = unsafe { ffi::vita2d_texture_get_width(ptr.as_ptr()) } as i32;
        let height = unsafe { ffi::vita2d_texture_get_height(ptr.as_ptr()) } as i32;
        Ok(Self {
            ptr,
            width,
            height,
        })
    }
}

impl Drop for Texture {
    fn drop(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_free_texture(self.ptr.as_ptr());
        }
    }
}

/// Color emoji sprite atlas + codepoint→cell lookup table. Loads
/// `twemoji.png` (a single PNG with 64×64 cells in a 16-column grid) and
/// exposes [`EmojiAtlas::lookup`] to find a codepoint's atlas cell.
///
/// Construct with [`EmojiAtlas::from_path`] at startup; pass a borrow
/// through `UiCtx` to screens that render emoji-bearing text.
pub struct EmojiAtlas {
    texture: Texture,
}

impl EmojiAtlas {
    pub fn from_path(path: &str) -> Result<Self, RenderError> {
        Ok(Self {
            texture: Texture::from_png_file(path)?,
        })
    }

    /// Look up a codepoint's atlas cell. `None` if the codepoint isn't
    /// in the bundled set; the caller falls back to TTF text rendering.
    pub fn lookup(&self, codepoint: u32) -> Option<(u16, u16)> {
        emoji_table::lookup(codepoint)
    }

    /// Underlying texture (for direct draw via `Frame::draw_texture_part_scale`).
    pub fn texture(&self) -> &Texture {
        &self.texture
    }
}

/// LRU cache of decoded image textures, keyed by source URL.
///
/// Owned by `main.rs`; passed to screens via `UiCtx::texture_cache` for
/// read-only lookup. Mutations (insert on response, eviction on
/// overflow) happen in the main loop after the worker drain — screens
/// don't mutate.
///
/// Capacity is in entries, not bytes. Each entry is one decoded
/// `Texture` (~37 KB at 96×96 RGBA, ~9 KB at 48×48 RGBA). Default 64
/// entries fits roughly 2.4 MB worst case; comfortably under the GXM
/// heap budget.
///
/// Eviction policy: simple LRU. `insert` pushes to the back; `touch`
/// promotes a hit to the back; on overflow, the front entry's
/// `Texture` is dropped (frees the GPU memory via `vita2d_free_texture`).
pub struct TextureCache {
    map: HashMap<String, Texture>,
    /// Insertion / access order. Front = oldest, back = newest.
    order: VecDeque<String>,
    capacity: usize,
}

impl TextureCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn contains(&self, url: &str) -> bool {
        self.map.contains_key(url)
    }

    pub fn get(&self, url: &str) -> Option<&Texture> {
        self.map.get(url)
    }

    /// Promote `url` to the most-recently-used position. Call from
    /// per-frame render code that observes a cache hit, so frequently
    /// drawn textures don't get evicted.
    pub fn touch(&mut self, url: &str) {
        if !self.map.contains_key(url) {
            return;
        }
        // Find and remove from current position.
        if let Some(pos) = self.order.iter().position(|u| u == url) {
            let s = self.order.remove(pos).expect("position is valid");
            self.order.push_back(s);
        }
    }

    /// Decode `bytes` (PNG or JPEG, auto-detected) and insert. If the
    /// URL already exists, replaces the entry (re-decoded). Evicts the
    /// oldest entry on overflow.
    pub fn insert(&mut self, url: String, bytes: &[u8]) -> Result<(), RenderError> {
        let texture = Texture::from_image_bytes(bytes)?;
        // If updating existing, drop the old entry's slot in order.
        if self.map.contains_key(&url) {
            if let Some(pos) = self.order.iter().position(|u| u == &url) {
                self.order.remove(pos);
            }
        } else if self.map.len() >= self.capacity {
            // At capacity — evict the LRU entry first. Drop the texture
            // explicitly via remove from the map (which calls Drop ->
            // vita2d_free_texture).
            if let Some(victim) = self.order.pop_front() {
                self.map.remove(&victim);
            }
        }
        self.map.insert(url.clone(), texture);
        self.order.push_back(url);
        Ok(())
    }

    /// Number of entries currently cached.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
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

    // ─── Texture drawing (Phase 3.4) ───────────────────────────────

    /// Draw `tex` at its native size with top-left at `(x, y)`.
    pub fn draw_texture(&mut self, tex: &Texture, x: f32, y: f32) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_draw_texture(tex.ptr.as_ptr(), x, y);
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (tex, x, y);
        }
    }

    /// Draw `tex` scaled by `(x_scale, y_scale)` with top-left at `(x, y)`.
    pub fn draw_texture_scale(
        &mut self,
        tex: &Texture,
        x: f32,
        y: f32,
        x_scale: f32,
        y_scale: f32,
    ) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_draw_texture_scale(tex.ptr.as_ptr(), x, y, x_scale, y_scale);
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (tex, x, y, x_scale, y_scale);
        }
    }

    /// Draw the `(src_w × src_h)` sub-rectangle of `tex` at source
    /// position `(src_x, src_y)`, placed at screen position `(x, y)` and
    /// scaled by `(x_scale, y_scale)`. Used for sprite-atlas rendering
    /// (e.g., color emoji glyphs from the Twemoji atlas).
    pub fn draw_texture_part_scale(
        &mut self,
        tex: &Texture,
        x: f32,
        y: f32,
        src_x: f32,
        src_y: f32,
        src_w: f32,
        src_h: f32,
        x_scale: f32,
        y_scale: f32,
    ) {
        #[cfg(target_os = "vita")]
        unsafe {
            ffi::vita2d_draw_texture_part_scale(
                tex.ptr.as_ptr(),
                x,
                y,
                src_x,
                src_y,
                src_w,
                src_h,
                x_scale,
                y_scale,
            );
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (tex, x, y, src_x, src_y, src_w, src_h, x_scale, y_scale);
        }
    }

    // ─── Emoji-aware text drawing (Phase 3.4) ──────────────────────
    //
    // These mirror the plain-text methods but accept an optional
    // [`EmojiAtlas`]. When `emoji = None`, behavior is identical to the
    // plain-text variant. When `Some`, codepoints in the bundled set are
    // rendered as textured quads from the atlas inline with the
    // surrounding text run.
    //
    // The wrap loop treats whitespace as the only word boundary; an
    // emoji codepoint in the middle of a word is rendered inline without
    // forcing a break. Emoji width contributes to the running line
    // width as `BASE_SIZE_PX * scale` pixels.

    /// Single-line emoji-aware draw. Returns the next-x position past
    /// the last drawn glyph or emoji.
    pub fn draw_text_with_emoji(
        &mut self,
        font: &Font,
        x: i32,
        y: i32,
        color: Color,
        scale: f32,
        text: &str,
        emoji: Option<&EmojiAtlas>,
    ) -> i32 {
        match emoji {
            None => self.draw_text(font, x, y, color, scale, text),
            Some(atlas) => self.draw_word_with_emoji(font, x, y, color, scale, text, atlas),
        }
    }

    /// Word-wrapped emoji-aware draw. Mirrors `draw_text_wrapped`'s
    /// structure but substitutes emoji-aware width measurement and
    /// glyph drawing per word.
    pub fn draw_text_wrapped_with_emoji(
        &mut self,
        font: &Font,
        x: i32,
        y: i32,
        max_w: i32,
        color: Color,
        scale: f32,
        text: &str,
        emoji: Option<&EmojiAtlas>,
    ) -> i32 {
        let Some(atlas) = emoji else {
            return self.draw_text_wrapped(font, x, y, max_w, color, scale, text);
        };
        if text.is_empty() {
            return 0;
        }
        let (_, ref_h) = self.measure_text(font, scale, "Hg");
        let line_h = ref_h + 4;
        let space_w = self.measure_text(font, scale, " ").0;
        let mut y_cursor = y;

        for paragraph in text.split('\n') {
            let mut line_words: Vec<&str> = Vec::new();
            let mut line_width: i32 = 0;
            for word in paragraph.split_whitespace() {
                let w = self.measure_word_with_emoji(font, scale, word, atlas);
                let needed = if line_words.is_empty() {
                    w
                } else {
                    space_w + w
                };
                if line_width + needed <= max_w {
                    line_words.push(word);
                    line_width += needed;
                    continue;
                }
                // Doesn't fit on the current line. Flush, start new line.
                if !line_words.is_empty() {
                    self.draw_words_with_emoji(
                        font, x, y_cursor, color, scale, &line_words, space_w, atlas,
                    );
                    y_cursor += line_h;
                }
                line_words = vec![word];
                line_width = w;
            }
            if !line_words.is_empty() {
                self.draw_words_with_emoji(
                    font, x, y_cursor, color, scale, &line_words, space_w, atlas,
                );
                y_cursor += line_h;
            }
        }
        y_cursor - y
    }

    /// Measurement-only counterpart to `draw_text_wrapped_with_emoji`.
    pub fn measure_text_wrapped_with_emoji(
        &self,
        font: &Font,
        max_w: i32,
        scale: f32,
        text: &str,
        emoji: Option<&EmojiAtlas>,
    ) -> i32 {
        let Some(atlas) = emoji else {
            return self.measure_text_wrapped(font, max_w, scale, text);
        };
        if text.is_empty() {
            return 0;
        }
        let (_, ref_h) = self.measure_text(font, scale, "Hg");
        let line_h = ref_h + 4;
        let space_w = self.measure_text(font, scale, " ").0;
        let mut y_cursor = 0;
        for paragraph in text.split('\n') {
            let mut line_width: i32 = 0;
            let mut has_words = false;
            for word in paragraph.split_whitespace() {
                let w = self.measure_word_with_emoji(font, scale, word, atlas);
                let needed = if !has_words { w } else { space_w + w };
                if line_width + needed <= max_w {
                    line_width += needed;
                    has_words = true;
                } else {
                    if has_words {
                        y_cursor += line_h;
                    }
                    line_width = w;
                    has_words = true;
                }
            }
            if has_words {
                y_cursor += line_h;
            }
        }
        y_cursor
    }

    // ─── Private helpers for emoji-aware rendering ────────────────

    /// Width of `word` (no whitespace, may contain emoji codepoints) at
    /// `scale`. Each emoji contributes one text-line-height of width
    /// (size in pixels matches the surrounding text size).
    fn measure_word_with_emoji(
        &self,
        font: &Font,
        scale: f32,
        word: &str,
        atlas: &EmojiAtlas,
    ) -> i32 {
        let emoji_w = self.emoji_render_size(scale);
        let mut total: i32 = 0;
        let mut text_buf = String::new();
        for ch in word.chars() {
            if atlas.lookup(ch as u32).is_some() {
                if !text_buf.is_empty() {
                    total += self.measure_text(font, scale, &text_buf).0;
                    text_buf.clear();
                }
                total += emoji_w;
            } else {
                text_buf.push(ch);
            }
        }
        if !text_buf.is_empty() {
            total += self.measure_text(font, scale, &text_buf).0;
        }
        total
    }

    /// Draw one mixed-emoji-and-text word at `(x, y)`. Returns the
    /// next-x past the last drawn glyph/emoji.
    ///
    /// Advances `current_x` via `measure_text`, NOT `draw_text`'s return
    /// value — vita2d's `font_draw_text` returns the relative width
    /// drawn (not the absolute pen position), which would cause every
    /// subsequent chained draw to pile up at line start. measure_text is
    /// authoritative for both PGF and TTF.
    fn draw_word_with_emoji(
        &mut self,
        font: &Font,
        x: i32,
        y: i32,
        color: Color,
        scale: f32,
        word: &str,
        atlas: &EmojiAtlas,
    ) -> i32 {
        let emoji_w = self.emoji_render_size(scale);
        let cell = emoji_table::CELL_PX as f32;
        let scale_factor = emoji_w as f32 / cell;
        let emoji_y_top = y - emoji_w; // baseline at y → bottom of emoji at y

        let mut current_x = x;
        let mut text_buf = String::new();
        for ch in word.chars() {
            if let Some((col, row)) = atlas.lookup(ch as u32) {
                if !text_buf.is_empty() {
                    self.draw_text(font, current_x, y, color, scale, &text_buf);
                    current_x += self.measure_text(font, scale, &text_buf).0;
                    text_buf.clear();
                }
                self.draw_texture_part_scale(
                    atlas.texture(),
                    current_x as f32,
                    emoji_y_top as f32,
                    col as f32 * cell,
                    row as f32 * cell,
                    cell,
                    cell,
                    scale_factor,
                    scale_factor,
                );
                current_x += emoji_w;
            } else {
                text_buf.push(ch);
            }
        }
        if !text_buf.is_empty() {
            self.draw_text(font, current_x, y, color, scale, &text_buf);
            current_x += self.measure_text(font, scale, &text_buf).0;
        }
        current_x
    }

    /// Draw a sequence of words (separated by `space_w`) on one line at
    /// `(x, y)`. Used by `draw_text_wrapped_with_emoji`.
    fn draw_words_with_emoji(
        &mut self,
        font: &Font,
        x: i32,
        y: i32,
        color: Color,
        scale: f32,
        words: &[&str],
        space_w: i32,
        atlas: &EmojiAtlas,
    ) {
        let mut current_x = x;
        for (i, word) in words.iter().enumerate() {
            if i > 0 {
                current_x += space_w;
            }
            current_x = self.draw_word_with_emoji(font, current_x, y, color, scale, word, atlas);
        }
    }

    /// Compute the on-screen rendered size (width and height) of an
    /// emoji glyph at the given text `scale`. Emoji match the surrounding
    /// text height — at scale 1.0 → 20 px (BASE_SIZE_PX) on Vita.
    #[inline]
    fn emoji_render_size(&self, scale: f32) -> i32 {
        #[cfg(target_os = "vita")]
        {
            scale_to_px(scale) as i32
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = scale;
            16
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

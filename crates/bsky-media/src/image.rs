//! CPU image decode for upload / thumbnail preparation.
//!
//! The GPU path (vita2d `load_PNG` / `load_JPEG`) decodes into a
//! full-resolution GPU texture; for a multi-MB photo that texture is
//! ~12 MB and reliably triggers a GPUCRASH on the Vita (a clean OOM would
//! fall back gracefully, but vita2d corrupts the GPU instead). So
//! oversized local images are decoded on the CPU here — PNG via libpng's
//! simplified read API (`libpng16.a` is already linked for vita2d/
//! FreeType), JPEG via turbojpeg — and only the final downscaled texture
//! is ever handed to the GPU. (We FFI into the linked C libs rather than
//! pull a Rust PNG decoder, whose inflate deps drag in `simd-adler32`,
//! which uses unstable 32-bit-ARM NEON intrinsics that don't build on our
//! Vita nightly.)

/// Decode PNG or JPEG bytes to a tightly-packed RGBA8 buffer `(rgba, w, h)`.
pub fn decode_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        decode_png(bytes)
    } else {
        crate::jpeg::decode_rgba(bytes)
    }
}

/// libpng's "simplified" read API: decodes any PNG (palette, grayscale,
/// 16-bit, interlaced, …) straight to a requested format. We ask for
/// RGBA8 so the output is always tightly-packed `w*h*4`.
#[cfg(target_os = "vita")]
mod png_ffi {
    use core::ffi::{c_int, c_void};

    /// Mirrors libpng's `png_image` (PNG_IMAGE_VERSION 1): a pointer plus
    /// seven `png_uint_32`s plus a 64-byte message buffer.
    #[repr(C)]
    pub struct PngImage {
        pub opaque: *mut c_void,
        pub version: u32,
        pub width: u32,
        pub height: u32,
        pub format: u32,
        pub flags: u32,
        pub colormap_entries: u32,
        pub warning_or_error: u32,
        pub message: [u8; 64],
    }

    pub const PNG_IMAGE_VERSION: u32 = 1;
    // PNG_FORMAT_RGBA = PNG_FORMAT_FLAG_COLOR(0x02) | PNG_FORMAT_FLAG_ALPHA(0x01).
    pub const PNG_FORMAT_RGBA: u32 = 0x03;

    unsafe extern "C" {
        pub fn png_image_begin_read_from_memory(
            image: *mut PngImage,
            memory: *const c_void,
            size: usize,
        ) -> c_int;
        pub fn png_image_finish_read(
            image: *mut PngImage,
            background: *const c_void,
            buffer: *mut c_void,
            row_stride: i32,
            colormap: *mut c_void,
        ) -> c_int;
        pub fn png_image_free(image: *mut PngImage);
    }
}

#[cfg(target_os = "vita")]
fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    use core::ffi::c_void;
    use png_ffi::*;
    unsafe {
        let mut img: PngImage = core::mem::zeroed();
        img.version = PNG_IMAGE_VERSION;
        if png_image_begin_read_from_memory(
            &mut img,
            bytes.as_ptr() as *const c_void,
            bytes.len(),
        ) == 0
        {
            return Err("png_image_begin_read_from_memory failed".into());
        }
        img.format = PNG_FORMAT_RGBA;
        let (w, h) = (img.width, img.height);
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        // row_stride 0 = natural (width * 4); background/colormap unused.
        let rc = png_image_finish_read(
            &mut img,
            core::ptr::null(),
            out.as_mut_ptr() as *mut c_void,
            0,
            core::ptr::null_mut(),
        );
        if rc == 0 {
            png_image_free(&mut img); // finish_read frees the control on success
            return Err("png_image_finish_read failed".into());
        }
        Ok((out, w, h))
    }
}

#[cfg(not(target_os = "vita"))]
fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let _ = bytes;
    Err("png decode is only available on Vita".into())
}

/// Downscale a tightly-packed RGBA buffer to fit within `max_w × max_h`
/// (nearest-neighbour, preserving aspect). Returns the input unchanged if
/// it already fits.
pub fn downscale_rgba(
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    max_w: u32,
    max_h: u32,
) -> (Vec<u8>, u32, u32) {
    if w <= max_w && h <= max_h {
        return (rgba, w, h);
    }
    let scale = (max_w as f32 / w as f32)
        .min(max_h as f32 / h as f32)
        .min(1.0);
    let tw = ((w as f32 * scale).round() as u32).max(1);
    let th = ((h as f32 * scale).round() as u32).max(1);
    let mut out = vec![0u8; (tw as usize) * (th as usize) * 4];
    for dy in 0..th {
        let sy = dy * h / th;
        for dx in 0..tw {
            let sx = dx * w / tw;
            let s = ((sy * w + sx) * 4) as usize;
            let d = ((dy * tw + dx) * 4) as usize;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    (out, tw, th)
}

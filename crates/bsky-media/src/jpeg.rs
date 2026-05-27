//! In-memory JPEG encoding via turbojpeg (software).
//!
//! Used to encode camera frames and downscaled oversized images before
//! upload. Software (CPU) encode is chosen over the hardware
//! `sceJpegEncoder` to avoid its physically-contiguous-buffer / CSC / DMA
//! complexity (the same class of code that caused GPU crashes elsewhere).
//! A single sub-2000px encode is well under a second on the Cortex-A9.
//!
//! Host builds return an error (no turbojpeg linked off-device).

#[cfg(target_os = "vita")]
mod ffi {
    use core::ffi::{c_int, c_uchar, c_ulong, c_void};

    #[allow(non_camel_case_types)] // matches the turbojpeg C type name
    pub type tjhandle = *mut c_void;

    unsafe extern "C" {
        pub fn tjInitCompress() -> tjhandle;
        pub fn tjCompress2(
            handle: tjhandle,
            srcBuf: *const c_uchar,
            width: c_int,
            pitch: c_int,
            height: c_int,
            pixelFormat: c_int,
            jpegBuf: *mut *mut c_uchar,
            jpegSize: *mut c_ulong,
            jpegSubsamp: c_int,
            jpegQual: c_int,
            flags: c_int,
        ) -> c_int;
        pub fn tjFree(buffer: *mut c_uchar);
        pub fn tjDestroy(handle: tjhandle) -> c_int;
    }

    // turbojpeg enum values (stable ABI): TJPF_RGBA = 7, TJSAMP_420 = 2.
    pub const TJPF_RGBA: c_int = 7;
    pub const TJSAMP_420: c_int = 2;
}

/// Encode a tightly-packed RGBA buffer (`w * h * 4` bytes) to a JPEG byte
/// vector at the given quality (1–100). 4:2:0 chroma subsampling.
#[cfg(target_os = "vita")]
pub fn encode_rgba(rgba: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, String> {
    let need = (w as usize) * (h as usize) * 4;
    if rgba.len() < need {
        return Err(format!("rgba buffer {} < needed {need}", rgba.len()));
    }
    unsafe {
        let handle = ffi::tjInitCompress();
        if handle.is_null() {
            return Err("tjInitCompress failed".into());
        }
        let mut jpeg_buf: *mut u8 = core::ptr::null_mut();
        let mut jpeg_size: core::ffi::c_ulong = 0;
        let rc = ffi::tjCompress2(
            handle,
            rgba.as_ptr(),
            w as i32,
            0, // pitch 0 = tightly packed (width * pixel size)
            h as i32,
            ffi::TJPF_RGBA,
            &mut jpeg_buf,
            &mut jpeg_size,
            ffi::TJSAMP_420,
            quality as i32,
            0,
        );
        if rc != 0 || jpeg_buf.is_null() {
            ffi::tjDestroy(handle);
            return Err(format!("tjCompress2 failed: rc={rc}"));
        }
        let out = core::slice::from_raw_parts(jpeg_buf, jpeg_size as usize).to_vec();
        ffi::tjFree(jpeg_buf);
        ffi::tjDestroy(handle);
        Ok(out)
    }
}

#[cfg(not(target_os = "vita"))]
pub fn encode_rgba(rgba: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, String> {
    let _ = (rgba, w, h, quality);
    Err("jpeg encode is only available on Vita".into())
}

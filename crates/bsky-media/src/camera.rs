//! Camera capture via `sceCamera` — front/back, 640×480, packed ABGR.
//!
//! The frame buffer is a PHYCONT memblock (the camera firmware DMAs into
//! it). ABGR8888 little-endian is `[R,G,B,A]` bytes, matching our RGBA
//! textures + turbojpeg's `TJPF_RGBA` (verify on hardware).
//!
//! Several `SceCameraInfo`/`SceCameraRead` fields take values that the
//! vitasdk-sys bindings don't expose as constants; they're set
//! conservatively here and the open/start/read results are logged so the
//! exact working configuration can be dialed in on-device.

#[cfg(target_os = "vita")]
use vitasdk_sys as sce;

pub const DEVICE_FRONT: i32 = 0;
pub const DEVICE_BACK: i32 = 1;
pub const CAM_W: u32 = 640;
pub const CAM_H: u32 = 480;

#[cfg(target_os = "vita")]
pub struct Camera {
    device: core::ffi::c_int,
    buf_uid: sce::SceUID,
    base: *mut u8,
    buf_len: usize,
}

#[cfg(target_os = "vita")]
impl Camera {
    /// Open + start `device` (`DEVICE_FRONT` / `DEVICE_BACK`).
    pub fn open(device: i32) -> Result<Camera, String> {
        use core::ffi::{c_char, c_void};
        unsafe {
            let buf_len = (CAM_W * CAM_H * 4) as usize;
            // PHYCONT buffer (1 MB-aligned base; round size up to 1 MB).
            const GRANULE: usize = 1024 * 1024;
            let total = (buf_len + GRANULE - 1) & !(GRANULE - 1);
            let name = b"bsky_cam\0".as_ptr() as *const c_char;
            let uid = sce::sceKernelAllocMemBlock(
                name,
                sce::SCE_KERNEL_MEMBLOCK_TYPE_USER_MAIN_PHYCONT_NC_RW,
                total as u32,
                core::ptr::null_mut(),
            );
            if uid < 0 {
                return Err(format!("camera AllocMemBlock failed: {uid:#x}"));
            }
            let mut base: *mut c_void = core::ptr::null_mut();
            let r = sce::sceKernelGetMemBlockBase(uid, &mut base);
            if r < 0 || base.is_null() {
                sce::sceKernelFreeMemBlock(uid);
                return Err(format!("camera GetMemBlockBase failed: {r:#x}"));
            }

            let mut info: sce::SceCameraInfo = core::mem::zeroed();
            info.size = core::mem::size_of::<sce::SceCameraInfo>() as u32;
            info.priority = sce::SCE_CAMERA_PRIORITY_SHARE as u16;
            info.format = sce::SCE_CAMERA_FORMAT_ABGR as u16;
            info.resolution = sce::SCE_CAMERA_RESOLUTION_640_480 as u16;
            info.framerate = sce::SCE_CAMERA_FRAMERATE_30_FPS as u16;
            info.width = CAM_W as u16;
            info.height = CAM_H as u16;
            info.range = 0;
            info.sizeIBase = buf_len as u32;
            info.pIBase = base;
            info.pitch = 0; // 0 = packed (width * 4); revisit if frames skew
            info.buffer = 0;

            let r = sce::sceCameraOpen(device, &mut info);
            bsky_log::log!(
                "camera: sceCameraOpen(dev={device}) -> {r:#x} (pitch field now {})",
                info.pitch
            );
            if r < 0 {
                sce::sceKernelFreeMemBlock(uid);
                return Err(format!("sceCameraOpen failed: {r:#x}"));
            }
            let r = sce::sceCameraStart(device);
            bsky_log::log!("camera: sceCameraStart(dev={device}) -> {r:#x}");
            if r < 0 {
                sce::sceCameraClose(device);
                sce::sceKernelFreeMemBlock(uid);
                return Err(format!("sceCameraStart failed: {r:#x}"));
            }

            Ok(Camera {
                device,
                buf_uid: uid,
                base: base as *mut u8,
                buf_len,
            })
        }
    }

    /// Read the latest frame. Returns the frame buffer as an RGBA slice
    /// (ABGR8888 byte layout), or `None` if no fresh frame is available.
    pub fn read_rgba(&mut self) -> Option<&[u8]> {
        unsafe {
            let mut rd: sce::SceCameraRead = core::mem::zeroed();
            rd.size = core::mem::size_of::<sce::SceCameraRead>() as u32;
            rd.mode = 0; // wait for next frame
            let r = sce::sceCameraRead(self.device, &mut rd);
            if r < 0 {
                return None;
            }
            // Frame is written into our pIBase buffer; some modes also echo
            // the buffer in rd.pIBase — prefer that if set.
            let ptr = if rd.pIBase.is_null() {
                self.base
            } else {
                rd.pIBase as *mut u8
            };
            Some(core::slice::from_raw_parts(ptr, self.buf_len))
        }
    }
}

#[cfg(target_os = "vita")]
impl Drop for Camera {
    fn drop(&mut self) {
        unsafe {
            sce::sceCameraStop(self.device);
            sce::sceCameraClose(self.device);
            sce::sceKernelFreeMemBlock(self.buf_uid);
        }
        bsky_log::log!("camera: closed dev={}", self.device);
    }
}

// ── Host stub ──────────────────────────────────────────────────────────
#[cfg(not(target_os = "vita"))]
pub struct Camera;

#[cfg(not(target_os = "vita"))]
impl Camera {
    pub fn open(_device: i32) -> Result<Camera, String> {
        Err("camera is only available on Vita".into())
    }
    pub fn read_rgba(&mut self) -> Option<&[u8]> {
        None
    }
}

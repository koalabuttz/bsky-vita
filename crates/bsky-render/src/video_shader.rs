//! Phase 5.3.x.1 — custom GXM fragment shader for color video.
//!
//! Pipeline:
//!
//!   sceAvPlayer YUV420P3 frame
//!       │ (3 plane memcpys)
//!       ▼
//!   YuvTexture (three U8_R000 GXM textures: Y, U, V — own PHYCONT memblocks)
//!       │
//!       ▼
//!   `draw_quad`: bind custom vertex+fragment programs (compiled via vitashark
//!   from the Cg source in `video_shader_src.rs`), bind 3 textures, allocate
//!   per-frame vertex buffer in vita2d's pool, sceGxmDraw against vita2d's
//!   GXM context.
//!
//! Why not vita2d's stock shader: 5.3.x found that vita2d's stock textured
//! fragment shader doesn't activate the GPU sampler-hardware CSC pipeline
//! for YUV-format textures — even YUV420P3_CSC0 renders blank. We control
//! our own fragment shader, so we sample three luma-format planes and apply
//! BT.601 limited-range matrix in float math. Sidesteps the CSC bit
//! interaction entirely.
//!
//! Why three textures, not one 3-plane: the CSC bit interaction is the
//! exact failure mode we're trying to avoid; three independent
//! `SCE_GXM_TEXTURE_FORMAT_U8_R000` textures sample as plain bytes. Stride
//! is `width` for Y, `width/2` for U/V — sceAvPlayer's H.264 decoder
//! produces this layout exactly (no padding).

#![cfg(target_os = "vita")]
#![allow(non_camel_case_types, non_snake_case)]

use core::ffi::{c_char, c_int, c_uint, c_void};
use core::mem;
use core::ptr;
use std::sync::OnceLock;

use vitasdk_sys::{
    sceDmacMemcpy, sceGxmDraw, sceGxmProgramFindParameterByName, sceGxmProgramParameterGetResourceIndex,
    sceGxmSetFragmentProgram, sceGxmSetFragmentTexture, sceGxmSetVertexProgram,
    sceGxmSetVertexStream, sceGxmShaderPatcherCreateFragmentProgram,
    sceGxmShaderPatcherCreateVertexProgram, sceGxmShaderPatcherRegisterProgram,
    sceGxmTextureSetMagFilter, sceGxmTextureSetMinFilter,
    sceGxmTextureInitLinearStrided, sceGxmTextureSetUAddrMode, sceGxmTextureSetVAddrMode,
    sceGxmMapMemory, sceGxmUnmapMemory,
    sceKernelAllocMemBlock, sceKernelFindMemBlockByAddr, sceKernelFreeMemBlock,
    sceKernelGetMemBlockBase, sceKernelLoadStartModule,
    sceShaccCgCompileProgram, sceShaccCgDestroyCompileOutput, sceShaccCgInitializeCallbackList,
    sceShaccCgInitializeCompileOptions, sceShaccCgSetDefaultAllocator, sceSysmoduleLoadModule,
    SceGxmFragmentProgram, SceGxmTexture, SceGxmVertexAttribute, SceGxmVertexProgram,
    SceGxmVertexStream, SceShaccCgCallbackList, SceShaccCgCompileOptions, SceShaccCgSourceFile,
    SceShaccCgSourceLocation,
    SCE_GXM_ATTRIBUTE_FORMAT_F32, SCE_GXM_INDEX_FORMAT_U16, SCE_GXM_INDEX_SOURCE_INDEX_16BIT,
    SCE_GXM_MEMORY_ATTRIB_READ, SCE_GXM_MULTISAMPLE_NONE,
    SCE_GXM_OUTPUT_REGISTER_FORMAT_UCHAR4, SCE_GXM_PRIMITIVE_TRIANGLE_STRIP,
    SCE_GXM_TEXTURE_ADDR_CLAMP, SCE_GXM_TEXTURE_FILTER_LINEAR, SCE_GXM_TEXTURE_FORMAT_U8_RRRR,
    SCE_KERNEL_MEMBLOCK_TYPE_USER_CDRAM_RW, SCE_SHACCCG_PROFILE_FP,
    SCE_SHACCCG_PROFILE_VP, SCE_SHACCCG_TRIVIAL, SCE_SYSMODULE_SHACCCG,
};

use crate::ffi;
use crate::video_shader_src::{VIDEO_YUV_FRAG, VIDEO_YUV_VERT};

/// Pointer to the SceShaccCgSourceFile our `open_file_cb` should
/// return. Set immediately before each `sceShaccCgCompileProgram` call;
/// SHACCCG runs synchronously single-threaded within the call.
static mut COMPILE_SOURCE_FILE: *const SceShaccCgSourceFile = ptr::null();

unsafe extern "C" fn open_file_cb(
    _file_name: *const c_char,
    _included_from: *const SceShaccCgSourceLocation,
    _compile_options: *const SceShaccCgCompileOptions,
    _error_string: *mut *const c_char,
) -> *mut SceShaccCgSourceFile {
    // SHACCCG asks us to "open" the source file. We've stashed our
    // pre-built SceShaccCgSourceFile in a static; just hand it back.
    // We never #include from another file, so any name resolves to the
    // single source we provide.
    COMPILE_SOURCE_FILE as *mut SceShaccCgSourceFile
}

unsafe extern "C" fn release_file_cb(
    _file: *const SceShaccCgSourceFile,
    _compile_options: *const SceShaccCgCompileOptions,
) {
    // No-op: the source file lives in our caller's stack frame.
}

/// Compile one shader directly via SHACCCG, bypassing libvitashark
/// (which mismatches our user-loaded SHACCCG ABI on this Vita —
/// returns SCE_KERNEL_ERROR_MODULEMGR_OLD_LIB from shark_init). Returns
/// the raw `SceGxmProgram` bytes copied out of SHACCCG's output buffer.
unsafe fn compile_shader(source: &str, profile: vitasdk_sys::SceShaccCgTargetProfile)
    -> Option<Vec<u8>>
{
    use std::ffi::CString;
    let Ok(source_c) = CString::new(source) else {
        bsky_log::log!("video shader: source CString conv failed (interior NUL?)");
        return None;
    };
    let filename = c"main.cg";
    let entry = c"main";

    let source_file = SceShaccCgSourceFile {
        fileName: filename.as_ptr(),
        text: source_c.as_ptr(),
        size: source_c.as_bytes().len() as u32,
    };

    let mut options: SceShaccCgCompileOptions = mem::zeroed();
    let r = sceShaccCgInitializeCompileOptions(&mut options);
    if r < 0 {
        bsky_log::log!("video shader: InitializeCompileOptions = 0x{:08x}", r as u32);
        return None;
    }
    options.mainSourceFile = filename.as_ptr();
    options.targetProfile = profile;
    options.entryFunctionName = entry.as_ptr();

    let mut callbacks: SceShaccCgCallbackList = mem::zeroed();
    sceShaccCgInitializeCallbackList(&mut callbacks, SCE_SHACCCG_TRIVIAL);
    callbacks.openFile = Some(open_file_cb);
    callbacks.releaseFile = Some(release_file_cb);

    COMPILE_SOURCE_FILE = &source_file;
    let output = sceShaccCgCompileProgram(&options, &callbacks, 0);
    COMPILE_SOURCE_FILE = ptr::null();

    if output.is_null() {
        bsky_log::log!("video shader: CompileProgram returned null output");
        return None;
    }
    let out_ref = &*output;

    // Drain any diagnostic messages so a syntax error isn't silent.
    if !out_ref.diagnostics.is_null() {
        for i in 0..out_ref.diagnosticCount as isize {
            let d = &*out_ref.diagnostics.offset(i);
            if !d.message.is_null() {
                let msg = core::ffi::CStr::from_ptr(d.message).to_string_lossy();
                bsky_log::log!(
                    "video shader: diag[{}] level={} code={} msg={}",
                    i, d.level, d.code, msg
                );
            }
        }
    }

    if out_ref.programData.is_null() || out_ref.programSize == 0 {
        bsky_log::log!(
            "video shader: empty program output (data={:p} size={})",
            out_ref.programData, out_ref.programSize
        );
        sceShaccCgDestroyCompileOutput(output);
        return None;
    }
    let blob: Vec<u8> = core::slice::from_raw_parts(
        out_ref.programData,
        out_ref.programSize as usize,
    ).to_vec();
    sceShaccCgDestroyCompileOutput(output);
    Some(blob)
}

/// Bytes per pixel for U8_RRRR textures = 1.
const PLANE_BPP: u32 = 1;
/// Memblock granularity for `USER_CDRAM_RW`: 256 KB. CDRAM is GPU-side
/// video memory — CPU writes are uncached on the CPU side, GPU sees
/// them immediately, and the GXM MMU maps it natively without the
/// quirks of mapping USER_RW (which can leave writes invisible to the
/// GPU even with UNCACHE flag). vita2d's own textures use CDRAM.
const MEMBLOCK_GRANULARITY: u32 = 256 * 1024;

#[inline]
fn round_up(value: u32, granularity: u32) -> u32 {
    (value + granularity - 1) & !(granularity - 1)
}

/// One luma-format texture: a `width × height` U8_RRRR plane backed by
/// a private CDRAM memblock that is sceGxmMapMemory-registered. We own
/// the memory; sceAvPlayer's decoder doesn't write here directly —
/// `upload` DMAs the plane in.
struct PlaneTexture {
    gxm_tex: SceGxmTexture,
    memblock_uid: i32,
    base: *mut u8,
    width: u32,
    height: u32,
    /// Texture stride in bytes — width rounded up to 32 bytes for GXM
    /// linear-texture sampler-hardware alignment requirements (the
    /// default `sceGxmTextureInitLinear` may pick a stride that
    /// doesn't match our tightly-packed memcpy if width isn't already
    /// 32-aligned, producing misaligned GPU reads).
    stride: u32,
}

impl PlaneTexture {
    fn create(name: &core::ffi::CStr, width: u32, height: u32) -> Result<Self, &'static str> {
        // Linear textures require 32-byte stride alignment (the
        // sampler hardware fetches in 32-byte bursts). For widths that
        // aren't already 32-aligned, we have to allocate the bigger
        // pitch and tell GXM the explicit stride via
        // sceGxmTextureInitLinearStrided.
        let stride = round_up(width, 32);
        let needed = stride * height * PLANE_BPP;
        let size = round_up(needed, MEMBLOCK_GRANULARITY);
        let uid = unsafe {
            sceKernelAllocMemBlock(
                name.as_ptr(),
                SCE_KERNEL_MEMBLOCK_TYPE_USER_CDRAM_RW,
                size,
                ptr::null_mut(),
            )
        };
        if uid < 0 {
            bsky_log::log!(
                "yuv plane alloc memblock {} {}x{} failed: 0x{:08x}",
                name.to_string_lossy(),
                width,
                height,
                uid as u32
            );
            return Err("memblock alloc");
        }
        let mut base: *mut c_void = ptr::null_mut();
        let r = unsafe { sceKernelGetMemBlockBase(uid, &mut base) };
        if r < 0 || base.is_null() {
            unsafe { sceKernelFreeMemBlock(uid); }
            return Err("memblock base");
        }
        // Map the memblock into the GXM MMU. CDRAM does NOT auto-map
        // (testing without this call crashes the GPU).
        let r = unsafe {
            sceGxmMapMemory(base, size, SCE_GXM_MEMORY_ATTRIB_READ)
        };
        if r < 0 && (r as u32) != 0x8000_0019 /* already mapped */ {
            bsky_log::log!(
                "video shader: sceGxmMapMemory failed: 0x{:08x}",
                r as u32
            );
            unsafe { sceKernelFreeMemBlock(uid); }
            return Err("gxmMapMemory");
        }
        let mut gxm_tex: SceGxmTexture = unsafe { mem::zeroed() };
        let r = unsafe {
            sceGxmTextureInitLinearStrided(
                &mut gxm_tex,
                base,
                SCE_GXM_TEXTURE_FORMAT_U8_RRRR,
                width,
                height,
                stride,
            )
        };
        if r < 0 {
            bsky_log::log!(
                "video shader: gxmTextureInitLinearStrided({}x{}, stride={}) failed: 0x{:08x}",
                width, height, stride, r as u32
            );
            unsafe {
                sceGxmUnmapMemory(base);
                sceKernelFreeMemBlock(uid);
            }
            return Err("gxmTextureInitLinearStrided");
        }
        unsafe {
            sceGxmTextureSetMinFilter(&mut gxm_tex, SCE_GXM_TEXTURE_FILTER_LINEAR);
            sceGxmTextureSetMagFilter(&mut gxm_tex, SCE_GXM_TEXTURE_FILTER_LINEAR);
            sceGxmTextureSetUAddrMode(&mut gxm_tex, SCE_GXM_TEXTURE_ADDR_CLAMP);
            sceGxmTextureSetVAddrMode(&mut gxm_tex, SCE_GXM_TEXTURE_ADDR_CLAMP);
        }
        Ok(Self {
            gxm_tex,
            memblock_uid: uid,
            base: base as *mut u8,
            width,
            height,
            stride,
        })
    }

    /// Copy `src` (with `src_pitch` row stride) into the texture's
    /// GPU-mapped CDRAM buffer. When src_pitch == dst_stride == width
    /// (the common 32-aligned case), uses a single hardware-DMA copy
    /// — much faster than CPU streaming from non-cached PHYCONT to
    /// uncached CDRAM. Otherwise falls back to row-by-row memcpy.
    fn upload(&self, src: &[u8], src_pitch: usize) {
        unsafe {
            let dst_stride = self.stride as usize;
            let row_bytes = self.width as usize;
            let h = self.height as usize;
            if src_pitch == dst_stride && row_bytes == src_pitch {
                // Single block — DMA from PHYCONT to CDRAM.
                let total = src_pitch * h;
                sceDmacMemcpy(
                    self.base as *mut c_void,
                    src.as_ptr() as *const c_void,
                    total as u32,
                );
            } else {
                for row in 0..h {
                    core::ptr::copy_nonoverlapping(
                        src.as_ptr().add(row * src_pitch),
                        self.base.add(row * dst_stride),
                        row_bytes,
                    );
                }
            }
        }
    }
}

impl Drop for PlaneTexture {
    fn drop(&mut self) {
        unsafe {
            sceGxmUnmapMemory(self.base as *mut c_void);
            sceKernelFreeMemBlock(self.memblock_uid);
        }
    }
}

/// A YUV420 video frame as three GXM-bindable luma-format textures.
/// Y at full resolution, U/V at half resolution per axis.
pub struct YuvTexture {
    y: PlaneTexture,
    u: PlaneTexture,
    v: PlaneTexture,
    /// Cached heap scratch buffer for NV12 chroma deinterleave. The
    /// upload reads sceAvPlayer's PHYCONT_NC_RW chroma plane in one
    /// block memcpy → cached scratch (fast NEON memcpy), then
    /// deinterleaves from cached scratch → uncached CDRAM textures.
    /// Streaming the deinterleave directly from non-cached PHYCONT
    /// to uncached CDRAM byte-by-byte locks up the GPU bus.
    chroma_scratch: Vec<u8>,
    width: u32,
    height: u32,
}

impl YuvTexture {
    pub fn create(width: u32, height: u32) -> Result<Self, crate::RenderError> {
        let cw = width / 2;
        let ch = height / 2;
        let y = PlaneTexture::create(c"yuv-y", width, height)
            .map_err(crate::RenderError::TextureLoad)?;
        let u = PlaneTexture::create(c"yuv-u", cw, ch)
            .map_err(crate::RenderError::TextureLoad)?;
        let v = PlaneTexture::create(c"yuv-v", cw, ch)
            .map_err(crate::RenderError::TextureLoad)?;
        // Force shader pipeline init now (lazy in ensure_pipeline).
        // If it fails, the YuvTexture is useless — fail fast at create time.
        if ensure_pipeline().is_none() {
            return Err(crate::RenderError::TextureLoad("shader pipeline init"));
        }
        // Pre-size the chroma scratch buffer for the NV12 plane.
        let chroma_bytes = (width * height / 2) as usize;
        let chroma_scratch = vec![0u8; chroma_bytes];
        Ok(Self { y, u, v, chroma_scratch, width, height })
    }

    pub fn upload(
        &mut self,
        y: &[u8],
        y_pitch: usize,
        uv: &[u8],
        uv_pitch: usize,
    ) {
        // Wait for the GPU to finish rendering whatever frame was last
        // submitted before we overwrite the texture data it samples
        // from. Without this, fast-moving video content can show
        // tearing as the CPU's mid-frame DMA races with the GPU's
        // ongoing texture fetches.
        unsafe { ffi::vita2d_wait_rendering_done(); }
        // Y: straight memcpy (unchanged).
        self.y.upload(y, y_pitch);
        // Step 1: hardware-DMA the entire NV12 chroma plane from
        // sceAvPlayer's non-cached PHYCONT buffer into our cached heap
        // scratch. The DMA frees the CPU during the transfer and is
        // much faster than streaming-read CPU memcpy.
        let n = uv.len().min(self.chroma_scratch.len());
        unsafe {
            sceDmacMemcpy(
                self.chroma_scratch.as_mut_ptr() as *mut c_void,
                uv.as_ptr() as *const c_void,
                n as u32,
            );
        }
        // Step 2: deinterleave from cached scratch into the U/V CDRAM
        // textures. To keep uncached-write count down we process 4
        // chroma pairs per inner iteration: read 8 source bytes (as
        // two unaligned u32s), bit-twiddle to pack 4 U bytes and 4 V
        // bytes into u32s, write each as a single u32 to CDRAM. ~4×
        // fewer uncached writes than byte-by-byte; lets write
        // combining coalesce 4-byte bursts.
        let chroma_w = self.u.width as usize;
        let chroma_h = self.u.height as usize;
        let u_dst_stride = self.u.stride as usize;
        let v_dst_stride = self.v.stride as usize;
        unsafe {
            for row in 0..chroma_h {
                let src_row = self.chroma_scratch.as_ptr().add(row * uv_pitch);
                let u_dst_row = self.u.base.add(row * u_dst_stride);
                let v_dst_row = self.v.base.add(row * v_dst_stride);
                let mut col = 0usize;
                while col + 4 <= chroma_w {
                    let lo = (src_row.add(col * 2) as *const u32).read_unaligned();
                    let hi = (src_row.add(col * 2 + 4) as *const u32).read_unaligned();
                    // lo bytes (LSB→MSB): U0, V0, U1, V1
                    // hi bytes: U2, V2, U3, V3
                    let u_packed = (lo & 0xFF)
                        | ((lo >> 8) & 0xFF00)
                        | ((hi & 0xFF) << 16)
                        | (((hi >> 8) & 0xFF00) << 16);
                    let v_packed = ((lo >> 8) & 0xFF)
                        | ((lo >> 16) & 0xFF00)
                        | (((hi >> 8) & 0xFF) << 16)
                        | (((hi >> 16) & 0xFF00) << 16);
                    (u_dst_row.add(col) as *mut u32).write_unaligned(u_packed);
                    (v_dst_row.add(col) as *mut u32).write_unaligned(v_packed);
                    col += 4;
                }
                while col < chroma_w {
                    *u_dst_row.add(col) = *src_row.add(col * 2);
                    *v_dst_row.add(col) = *src_row.add(col * 2 + 1);
                    col += 1;
                }
            }
        }
    }

    /// Fill the planes with a synthetic test gradient. Useful pre-
    /// integration to verify the shader pipeline draws colour without
    /// depending on sceAvPlayer being live.
    ///
    /// Pattern: Y ramps left-to-right (0→255), U ramps top-to-bottom
    /// (0→255), V is constant 128 (no chroma shift on the V axis).
    /// Result: a horizontal gradient from dark to light, with a
    /// vertical gradient of cool→warm tint.
    pub fn upload_test_pattern(&self) {
        unsafe {
            // Y plane
            let yw = self.y.width as usize;
            let yh = self.y.height as usize;
            for row in 0..yh {
                let line = self.y.base.add(row * yw);
                for col in 0..yw {
                    let v = ((col * 255) / yw.max(1)) as u8;
                    *line.add(col) = v;
                }
            }
            // U plane (vertical gradient)
            let cw = self.u.width as usize;
            let ch = self.u.height as usize;
            for row in 0..ch {
                let line = self.u.base.add(row * cw);
                let v = ((row * 255) / ch.max(1)) as u8;
                for col in 0..cw {
                    *line.add(col) = v;
                }
            }
            // V plane (neutral)
            for row in 0..ch {
                let line = self.v.base.add(row * cw);
                for col in 0..cw {
                    *line.add(col) = 128;
                }
            }
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
}

/// Compiled, registered shader pipeline. One-shot init; lives for the
/// process. The `SceGxmProgram*` blobs are kept in process-static
/// `Vec<u8>` so SHACCCG's output buffer can be freed between compiles
/// while the program metadata stays valid for the patcher.
pub struct VideoShaderPipeline {
    /// Program data copied out of SHACCCG — referenced by the
    /// patcher-registered programs, must outlive them.
    _vert_blob: Vec<u8>,
    _frag_blob: Vec<u8>,
    vert_program: *mut SceGxmVertexProgram,
    frag_program: *mut SceGxmFragmentProgram,
    /// Texture-unit resource indices for the three samplers in the
    /// fragment shader. Looked up at init via
    /// `sceGxmProgramFindParameterByName` — declaration order doesn't
    /// guarantee unit 0/1/2 with SHACCCG.
    y_tex_unit: u32,
    u_tex_unit: u32,
    v_tex_unit: u32,
}

// SAFETY: we never mutate the cached pipeline once initialized; the
// programs are owned by vita2d's shader patcher (process-global). Pointer
// access is read-only at draw time.
unsafe impl Send for VideoShaderPipeline {}
unsafe impl Sync for VideoShaderPipeline {}

static PIPELINE: OnceLock<Option<VideoShaderPipeline>> = OnceLock::new();

/// Lazy-init the shader pipeline (load SHACCCG, compile vert+frag,
/// register with vita2d's patcher). Returns `None` on any failure;
/// failure detail is logged via `bsky_log!`.
pub fn ensure_pipeline() -> Option<&'static VideoShaderPipeline> {
    PIPELINE.get_or_init(init_pipeline).as_ref()
}

fn init_pipeline() -> Option<VideoShaderPipeline> {
    // 1. Prefer bundled precompiled GXP. Needs no libshacccg.suprx, so
    //    color video works on every console. The .gxp blobs are a capture
    //    of the runtime compile below; GXP is final GPU bytecode, identical
    //    across all Vitas (same SGX543), so a blob compiled once on any
    //    device runs everywhere.
    if let Some((vert, frag)) = load_bundled_gxp() {
        bsky_log::log!(
            "video shader: loading bundled GXP ({}B vert + {}B frag, no SHACCCG)",
            vert.len(),
            frag.len()
        );
        if let Some(p) = build_pipeline_from_blobs(vert, frag) {
            return Some(p);
        }
        bsky_log::log!("video shader: bundled GXP failed to register; trying runtime compile");
    }

    // 2. Runtime compile via libshacccg.suprx — the source of truth used to
    //    capture the bundled blobs, and the path on dev builds before the
    //    .gxp is bundled. Falls through to greyscale (None) if absent.
    let (vert_blob, frag_blob) = unsafe {
        // Load SHACCCG sysmodule. The system path
        // (sceSysmoduleLoadModule) only works on setups where a
        // substitution plugin like ShaRKBR33D is installed alongside
        // libshacccg.suprx; otherwise it returns 0x805a1000 (file not
        // on device). Fall back to direct sceKernelLoadStartModule
        // against the canonical install path — same end state, the
        // SHACCCG export library gets registered either way.
        let r = sceSysmoduleLoadModule(SCE_SYSMODULE_SHACCCG);
        if r < 0 {
            let path = c"ur0:data/libshacccg.suprx";
            let mut status: c_int = 0;
            let mod_id = sceKernelLoadStartModule(
                path.as_ptr(),
                0,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut status,
            );
            if mod_id < 0 {
                // Try the alternate canonical path before giving up.
                let alt = c"ur0:data/external/libshacccg.suprx";
                let mod_id2 = sceKernelLoadStartModule(
                    alt.as_ptr(),
                    0,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut status,
                );
                if mod_id2 < 0 {
                    bsky_log::log!(
                        "video shader: SHACCCG load failed (sysmodule=0x{:08x}, \
                         primary=0x{:08x}, alt=0x{:08x}); libshacccg.suprx not installed",
                        r as u32, mod_id as u32, mod_id2 as u32
                    );
                    return None;
                }
            }
        }
        // SHACCCG needs an allocator callback before any compile call;
        // without it, internal scratch allocs return null and the
        // compiler segfaults inside CompileProgram. Hand it newlib's
        // malloc/free (same heap as the rest of the app).
        let r = sceShaccCgSetDefaultAllocator(Some(ffi::malloc), Some(ffi::free));
        if r < 0 {
            bsky_log::log!("video shader: SetDefaultAllocator failed: 0x{:08x}", r as u32);
            return None;
        }

        // Compile vert+frag directly via SHACCCG (no vitashark —
        // shark_init returns SCE_KERNEL_ERROR_MODULEMGR_OLD_LIB on
        // setups where libshacccg.suprx is loaded as a user module).
        let Some(vert_blob) = compile_shader(VIDEO_YUV_VERT, SCE_SHACCCG_PROFILE_VP) else {
            bsky_log::log!("video shader: vertex compile failed");
            return None;
        };
        let Some(frag_blob) = compile_shader(VIDEO_YUV_FRAG, SCE_SHACCCG_PROFILE_FP) else {
            bsky_log::log!("video shader: fragment compile failed");
            return None;
        };
        (vert_blob, frag_blob)
    };

    // Capture the freshly-compiled blobs (one-time, best-effort) so they
    // can be pulled off the device and bundled into app/static/.
    capture_gxp(&vert_blob, &frag_blob);

    build_pipeline_from_blobs(vert_blob, frag_blob)
}

/// Read the bundled precompiled GXP blobs packed into the VPK at `app0:`.
/// Returns the `(vertex, fragment)` program bytes when both are present
/// and non-empty, else `None` (dev builds before capture). `std::fs::read`
/// returns a clean `Err` on a missing file — unlike vita2d's loaders,
/// which crash — so probing is safe.
fn load_bundled_gxp() -> Option<(Vec<u8>, Vec<u8>)> {
    let vert = std::fs::read("app0:video_yuv_v.gxp").ok()?;
    let frag = std::fs::read("app0:video_yuv_f.gxp").ok()?;
    if vert.is_empty() || frag.is_empty() {
        return None;
    }
    Some((vert, frag))
}

/// One-time capture of a successful runtime compile: write the GXP blobs
/// to the data dir so they can be pulled off the device (`make fetch-gxp`)
/// and bundled into `app/static/`, after which `load_bundled_gxp` serves
/// them and libshacccg.suprx is no longer needed by anyone. No-op once
/// both files exist; best-effort (a failed write just means re-capture).
fn capture_gxp(vert: &[u8], frag: &[u8]) {
    const VPATH: &str = "ux0:data/BSKY00001/video_yuv_v.gxp";
    const FPATH: &str = "ux0:data/BSKY00001/video_yuv_f.gxp";
    if std::fs::metadata(VPATH).is_ok() && std::fs::metadata(FPATH).is_ok() {
        return;
    }
    let ok_v = std::fs::write(VPATH, vert).is_ok();
    let ok_f = std::fs::write(FPATH, frag).is_ok();
    bsky_log::log!(
        "video shader: captured GXP (vert {}B ok={}, frag {}B ok={}) -> {} ; \
         FTP both into app/static/ to bundle",
        vert.len(), ok_v, frag.len(), ok_f, VPATH
    );
}

/// Register two GXP program blobs with vita2d's shader patcher and build
/// the draw pipeline. Shared by the bundled-GXP and runtime-compile paths
/// (the blobs are byte-identical either way). Takes ownership of the blobs
/// and moves them into the returned struct — the patcher's registered
/// programs reference that memory, so it must outlive them (moving a `Vec`
/// keeps its heap buffer fixed, so the raw pointers stay valid).
fn build_pipeline_from_blobs(
    vert_blob: Vec<u8>,
    frag_blob: Vec<u8>,
) -> Option<VideoShaderPipeline> {
    unsafe {
        // Register programs with vita2d's shader patcher.
        let patcher = ffi::vita2d_get_shader_patcher();
        if patcher.is_null() {
            bsky_log::log!("video shader: vita2d patcher null");
            return None;
        }
        let vert_program_header = vert_blob.as_ptr() as *const _;
        let frag_program_header = frag_blob.as_ptr() as *const _;

        let mut vert_id: vitasdk_sys::SceGxmShaderPatcherId = ptr::null_mut();
        let r = sceGxmShaderPatcherRegisterProgram(patcher, vert_program_header, &mut vert_id);
        if r < 0 {
            bsky_log::log!("video shader: patcher register vert failed: 0x{:08x}", r as u32);
            return None;
        }
        let mut frag_id: vitasdk_sys::SceGxmShaderPatcherId = ptr::null_mut();
        let r = sceGxmShaderPatcherRegisterProgram(patcher, frag_program_header, &mut frag_id);
        if r < 0 {
            bsky_log::log!("video shader: patcher register frag failed: 0x{:08x}", r as u32);
            return None;
        }

        // Find the vertex attribute parameters (POSITION, TEXCOORD0).
        let pos_param = sceGxmProgramFindParameterByName(
            vert_program_header,
            c"position".as_ptr(),
        );
        let tex_param = sceGxmProgramFindParameterByName(
            vert_program_header,
            c"texcoord".as_ptr(),
        );
        if pos_param.is_null() || tex_param.is_null() {
            bsky_log::log!(
                "video shader: vertex program missing position/texcoord param: pos={:p} tex={:p}",
                pos_param,
                tex_param
            );
            return None;
        }

        // Vertex layout: position (float2) + texcoord (float2) = 16 bytes.
        let attributes = [
            SceGxmVertexAttribute {
                streamIndex: 0,
                offset: 0,
                format: SCE_GXM_ATTRIBUTE_FORMAT_F32 as u8,
                componentCount: 2,
                regIndex: sceGxmProgramParameterGetResourceIndex(pos_param) as u16,
            },
            SceGxmVertexAttribute {
                streamIndex: 0,
                offset: 8,
                format: SCE_GXM_ATTRIBUTE_FORMAT_F32 as u8,
                componentCount: 2,
                regIndex: sceGxmProgramParameterGetResourceIndex(tex_param) as u16,
            },
        ];
        let streams = [SceGxmVertexStream {
            stride: 16,
            indexSource: SCE_GXM_INDEX_SOURCE_INDEX_16BIT as u16,
        }];

        let mut vert_program: *mut SceGxmVertexProgram = ptr::null_mut();
        let r = sceGxmShaderPatcherCreateVertexProgram(
            patcher,
            vert_id,
            attributes.as_ptr(),
            attributes.len() as u32,
            streams.as_ptr(),
            streams.len() as u32,
            &mut vert_program,
        );
        if r < 0 {
            bsky_log::log!("video shader: createVertexProgram failed: 0x{:08x}", r as u32);
            return None;
        }
        let mut frag_program: *mut SceGxmFragmentProgram = ptr::null_mut();
        let r = sceGxmShaderPatcherCreateFragmentProgram(
            patcher,
            frag_id,
            SCE_GXM_OUTPUT_REGISTER_FORMAT_UCHAR4,
            SCE_GXM_MULTISAMPLE_NONE,
            ptr::null(), // no blending — opaque video
            vert_program_header,
            &mut frag_program,
        );
        if r < 0 {
            bsky_log::log!("video shader: createFragmentProgram failed: 0x{:08x}", r as u32);
            return None;
        }

        // Look up the three samplers in the fragment program — Cg
        // sampler params don't reliably map to texture units 0/1/2 in
        // declaration order. The resource index of a sampler param IS
        // the texture unit to pass to sceGxmSetFragmentTexture. Don't
        // FAIL on null lookups — for debug shaders that don't use
        // samplers, they may be absent. Log what we found and let the
        // draw try anyway.
        let y_param = sceGxmProgramFindParameterByName(frag_program_header, c"y_tex".as_ptr());
        let u_param = sceGxmProgramFindParameterByName(frag_program_header, c"u_tex".as_ptr());
        let v_param = sceGxmProgramFindParameterByName(frag_program_header, c"v_tex".as_ptr());
        let y_tex_unit = if y_param.is_null() { 0 } else { sceGxmProgramParameterGetResourceIndex(y_param) };
        let u_tex_unit = if u_param.is_null() { 1 } else { sceGxmProgramParameterGetResourceIndex(u_param) };
        let v_tex_unit = if v_param.is_null() { 2 } else { sceGxmProgramParameterGetResourceIndex(v_param) };
        bsky_log::log!(
            "video shader: pipeline ready (vert={}B frag={}B; tex units y={} u={} v={})",
            vert_blob.len(),
            frag_blob.len(),
            y_tex_unit,
            u_tex_unit,
            v_tex_unit,
        );
        Some(VideoShaderPipeline {
            _vert_blob: vert_blob,
            _frag_blob: frag_blob,
            y_tex_unit,
            u_tex_unit,
            v_tex_unit,
            vert_program,
            frag_program,
        })
    }
}

/// Vertex laid out to match `streams[0].stride = 16`. Two `f32`s for
/// position (NDC), two `f32`s for texcoord ([0,1]).
#[repr(C)]
#[derive(Copy, Clone)]
struct VideoVert {
    x: f32,
    y: f32,
    u: f32,
    v: f32,
}

/// Issue a draw of `tex` to the destination rectangle in screen pixels.
/// `screen_w`/`screen_h` are the framebuffer dimensions used to convert
/// to NDC.
pub fn draw_quad(
    pipeline: &VideoShaderPipeline,
    tex: &YuvTexture,
    dest_x: f32,
    dest_y: f32,
    dest_w: f32,
    dest_h: f32,
    screen_w: f32,
    screen_h: f32,
) {
    unsafe {
        let ctx = ffi::vita2d_get_context();
        if ctx.is_null() {
            return;
        }
        // Allocate the four-vertex strip in vita2d's per-frame scratch
        // pool. Pool resets each frame; no leak.
        let vbuf = ffi::vita2d_pool_memalign(
            (mem::size_of::<VideoVert>() * 4) as c_uint,
            mem::align_of::<VideoVert>() as c_uint,
        ) as *mut VideoVert;
        if vbuf.is_null() {
            return;
        }
        // Convert pixel rect to NDC. Vita's framebuffer origin is
        // top-left in vita2d's projection (matches libvita2d's stock
        // shader), so NDC y is flipped.
        let to_ndc_x = |px: f32| (px / screen_w) * 2.0 - 1.0;
        let to_ndc_y = |py: f32| 1.0 - (py / screen_h) * 2.0;
        let x0 = to_ndc_x(dest_x);
        let y0 = to_ndc_y(dest_y);
        let x1 = to_ndc_x(dest_x + dest_w);
        let y1 = to_ndc_y(dest_y + dest_h);
        // Triangle strip: TL, TR, BL, BR (vita2d's draw order).
        // UV (0,0) at TL, (1,1) at BR.
        *vbuf.add(0) = VideoVert { x: x0, y: y0, u: 0.0, v: 0.0 };
        *vbuf.add(1) = VideoVert { x: x1, y: y0, u: 1.0, v: 0.0 };
        *vbuf.add(2) = VideoVert { x: x0, y: y1, u: 0.0, v: 1.0 };
        *vbuf.add(3) = VideoVert { x: x1, y: y1, u: 1.0, v: 1.0 };

        sceGxmSetVertexProgram(ctx, pipeline.vert_program);
        sceGxmSetFragmentProgram(ctx, pipeline.frag_program);
        sceGxmSetFragmentTexture(ctx, pipeline.y_tex_unit, &tex.y.gxm_tex);
        sceGxmSetFragmentTexture(ctx, pipeline.u_tex_unit, &tex.u.gxm_tex);
        sceGxmSetFragmentTexture(ctx, pipeline.v_tex_unit, &tex.v.gxm_tex);
        sceGxmSetVertexStream(ctx, 0, vbuf as *const c_void);
        sceGxmDraw(
            ctx,
            SCE_GXM_PRIMITIVE_TRIANGLE_STRIP,
            SCE_GXM_INDEX_FORMAT_U16,
            ffi::vita2d_get_linear_indices() as *const c_void,
            4,
        );
    }
}

// Suppress unused_import warnings on items conditionally used.
#[allow(dead_code)]
fn _unused() {
    let _ = sceKernelFindMemBlockByAddr;
}

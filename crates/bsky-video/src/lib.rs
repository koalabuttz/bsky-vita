//! Safe wrappers around `sceAvPlayer` (hardware H.264 + AAC decoder)
//! and `sceAudioOut` (PCM output) for Bluesky video embed playback.
//!
//! `sceAvPlayer` runs its own decoder threads internally; the safe API
//! here is "hand it a file path, then poll for frames + audio chunks
//! each render-loop iteration." The library demands a memory-allocator
//! struct of four `extern "C" fn` callbacks; we route those into Rust's
//! global allocator so heap-tracking stays consistent across the app.
//!
//! Phase 5.3 scope: open + play + pause/resume + seek + drop. Out of
//! scope: looping, captions, multi-track audio selection, HLS via
//! `SceAvPlayerFileReplacement` (filed for 5.3.x — see
//! `video_decisions.md`).
//!
//! ## Host fallback
//!
//! On non-Vita targets every constructor returns
//! `Err(VideoError::NotOnVita)` so the rest of the codebase can
//! `cargo check` without the SDK present.

#![allow(clippy::needless_doctest_main)]

use std::ffi::CString;

#[cfg(target_os = "vita")]
use std::alloc::{alloc, dealloc, Layout};

#[cfg(target_os = "vita")]
use vitasdk_sys as sce;

#[derive(Debug)]
pub enum VideoError {
    /// `sceAvPlayerInit` returned an invalid handle.
    InitFailed(i32),
    /// `sceAvPlayerAddSource` rejected the file.
    AddSourceFailed(i32),
    /// File path contains a NUL byte.
    InvalidPath,
    /// `sceAudioOutOpenPort` returned a negative status.
    AudioOpenFailed(i32),
    /// Building this struct on a non-Vita target.
    NotOnVita,
}

impl core::fmt::Display for VideoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VideoError::InitFailed(code) => write!(f, "sceAvPlayerInit failed: {code:#x}"),
            VideoError::AddSourceFailed(code) => {
                write!(f, "sceAvPlayerAddSource failed: {code:#x}")
            }
            VideoError::InvalidPath => write!(f, "video path contains a NUL byte"),
            VideoError::AudioOpenFailed(code) => {
                write!(f, "sceAudioOutOpenPort failed: {code:#x}")
            }
            VideoError::NotOnVita => write!(f, "video playback is only available on Vita"),
        }
    }
}

impl core::error::Error for VideoError {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PlayerState {
    /// Playing forward.
    Playing,
    /// User-initiated pause.
    Paused,
    /// Reached EOF; no more frames will arrive.
    Eof,
}

/// One decoded video frame in NV12 layout. Borrowed from the player;
/// valid only until the next call to `next_video_frame`.
///
/// sceAvPlayer outputs NV12 (a.k.a. YUV420P2): a Y plane followed by
/// an interleaved UV plane (`U V U V U V …`) at half-vertical and
/// half-horizontal resolution. Until 5.3.x.1 we mistakenly assumed
/// YUV420P3 (planar U + planar V) — splitting the interleaved UV in
/// half made both halves look like ~128-clustered noise (the chroma
/// readback signature `[93, 6a, 93, 6a, …]` was the giveaway).
pub struct YuvFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// Microsecond timestamp from the source media.
    pub timestamp_us: u64,
    pub y: &'a [u8],
    pub y_pitch: usize,
    /// Interleaved UV plane: `width × (height/2)` bytes, row pitch =
    /// `width`, each row contains `width/2` (U, V) byte pairs. The
    /// renderer is responsible for deinterleaving when binding two
    /// luma-format textures.
    pub uv: &'a [u8],
    pub uv_pitch: usize,
}

/// One chunk of decoded audio samples (16-bit PCM, interleaved per
/// channel). Borrowed from the player.
pub struct AudioChunk<'a> {
    pub samples: &'a [i16],
    pub sample_rate: u32,
    pub channel_count: u16,
}

// ─── Player ────────────────────────────────────────────────────────────

/// One playing media stream. Hardware decoders run in `sceAvPlayer`'s
/// internal threads; the main-thread caller polls
/// `next_video_frame` / `next_audio_samples` per frame.
pub struct VideoPlayer {
    #[cfg(target_os = "vita")]
    handle: sce::SceAvPlayerHandle,
    /// Heap-allocated file-replacement state passed to sceAvPlayer
    /// via `fileReplacement.objectPointer`. Reclaimed in Drop.
    #[cfg(target_os = "vita")]
    file_state: *mut FileState,
    state: PlayerState,
    /// Latest video frame's metadata (so callers don't need to read
    /// the union themselves). Updated each `next_video_frame` call.
    video_w: u32,
    video_h: u32,
    /// `true` after we've decoded at least one video frame. Used to
    /// distinguish the warmup window (no frames yet, isActive == 0)
    /// from genuine EOF (frames seen, isActive now 0).
    seen_frame: bool,
    /// Diagnostic tick counter — every ~1 second's worth of polls,
    /// log isActive + currentTime so we can see whether decode is
    /// progressing at all.
    poll_count: u32,
}

/// Per-player file-replacement state. Pointer to this lives in
/// `SceAvPlayerFileReplacement.objectPointer`; the four callbacks
/// dereference it.
///
/// `read_lock` serializes the lseek + read pair. sceAvPlayer's
/// demuxer + decoder threads call `av_file_read` concurrently;
/// without a lock, one thread's lseek can be clobbered by another
/// before its read fires. (sceIoPread would be atomic and avoid the
/// lock, but its linker stub is unreliable in vitasdk-sys 0.3.3 —
/// the eboot fails to load.)
#[cfg(target_os = "vita")]
struct FileState {
    fd: sce::SceUID,
    size: u64,
    read_lock: std::sync::Mutex<()>,
}

impl VideoPlayer {
    /// Open `path` (an MP4/H.264 file). Loads the AvPlayer sysmodule
    /// on first call; subsequent calls are idempotent. Returns
    /// `Err` on non-Vita targets, malformed paths, or decoder
    /// rejection (e.g. HEVC, AV1).
    pub fn open(path: &str) -> Result<Self, VideoError> {
        let _cpath = CString::new(path).map_err(|_| VideoError::InvalidPath)?;
        #[cfg(target_os = "vita")]
        unsafe {
            // `SCE_SYSMODULE_AVPLAYER = 76`. Load is idempotent.
            let load_r = sce::sceSysmoduleLoadModule(sce::SCE_SYSMODULE_AVPLAYER);
            bsky_log::log!("video: sysmodule load r={load_r:#x}");

            // File state for the file-replacement callbacks. fd
            // starts -1; libavplayer calls `av_file_open` inside
            // AddSource and we sceIoOpen there.
            let file_state = Box::into_raw(Box::new(FileState {
                fd: -1,
                size: 0,
                read_lock: std::sync::Mutex::new(()),
            }));

            let mut init: sce::SceAvPlayerInitData = core::mem::zeroed();
            init.memoryReplacement.allocate = Some(av_alloc);
            init.memoryReplacement.deallocate = Some(av_free);
            // Texture allocator must return GPU-mapped memory — the
            // H.264 hardware decoder DMAs decoded frames in, and the
            // Rust heap is CPU-only (not visible to the GPU MMU).
            init.memoryReplacement.allocateTexture = Some(av_alloc_texture);
            init.memoryReplacement.deallocateTexture = Some(av_free_texture);
            init.fileReplacement.objectPointer = file_state as *mut core::ffi::c_void;
            init.fileReplacement.open = Some(av_file_open);
            init.fileReplacement.close = Some(av_file_close);
            init.fileReplacement.readOffset = Some(av_file_read);
            init.fileReplacement.size = Some(av_file_size);
            // Event callback emits state transitions + warnings.
            // Critical for diagnosing why playback never starts.
            init.eventReplacement.eventCallback = Some(av_event);
            // Higher-priority decoder threads (lower number = higher
            // pri on Vita). Default 0xA0 was likely getting starved by
            // the 0x70-priority render loop. 0x10 puts decoder ahead.
            init.basePriority = 0x10;
            init.numOutputVideoFrameBuffers = 4;
            // Auto-start when the demuxer has enough data. Calling
            // sceAvPlayerStart explicitly post-AddSource returned
            // 0x806a0002 (INVALID_STATE) regardless of autoStart —
            // demuxer load is async, so Start can't be called
            // synchronously. Trust autoStart instead.
            init.autoStart = true as sce::SceBool;
            // debugLevel=4 enables verbose internal logging. May
            // surface decoder-rejection messages we'd otherwise miss.
            init.debugLevel = 4;

            let handle = sce::sceAvPlayerInit(&mut init);
            bsky_log::log!("video: sceAvPlayerInit -> {handle:#x}");
            if handle == 0 {
                drop(Box::from_raw(file_state));
                return Err(VideoError::InitFailed(handle));
            }

            bsky_log::log!("video: addSource path={path}");
            let r = sce::sceAvPlayerAddSource(handle, _cpath.as_ptr());
            bsky_log::log!("video: sceAvPlayerAddSource -> {r:#x}");
            if r < 0 {
                sce::sceAvPlayerClose(handle);
                drop(Box::from_raw(file_state));
                return Err(VideoError::AddSourceFailed(r));
            }
            // No explicit Start — autoStart=true kicks in once the
            // demuxer has buffered enough data.

            Ok(VideoPlayer {
                handle,
                file_state,
                state: PlayerState::Playing,
                video_w: 0,
                video_h: 0,
                seen_frame: false,
                poll_count: 0,
            })
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = path;
            Err(VideoError::NotOnVita)
        }
    }

    /// Pull the next decoded video frame, if one is ready. The
    /// returned slice references sceAvPlayer-owned memory and is
    /// invalidated by the next call.
    pub fn next_video_frame(&mut self) -> Option<YuvFrame<'_>> {
        #[cfg(target_os = "vita")]
        unsafe {
            // Don't gate on isActive — the demuxer may still be in
            // the warmup window after Start. Track EOF separately
            // (see `tick_eof`).
            self.poll_count = self.poll_count.wrapping_add(1);
            if self.poll_count % 60 == 1 {
                let active = sce::sceAvPlayerIsActive(self.handle);
                let now = sce::sceAvPlayerCurrentTime(self.handle);
                bsky_log::log!(
                    "video: poll #{} isActive={active} t={now}us seen_frame={}",
                    self.poll_count,
                    self.seen_frame,
                );
            }
            let mut info: sce::SceAvPlayerFrameInfo = core::mem::zeroed();
            let got = sce::sceAvPlayerGetVideoData(self.handle, &mut info);
            if got == 0 {
                self.tick_eof();
                return None;
            }
            if !self.seen_frame {
                let v = info.details.video;
                bsky_log::log!(
                    "video: first frame {}x{} ts={}us",
                    v.width,
                    v.height,
                    info.timeStamp,
                );
            }
            self.seen_frame = true;
            let video = info.details.video;
            let w = video.width;
            let h = video.height;
            self.video_w = w;
            self.video_h = h;
            // sceAvPlayer outputs NV12: Y plane (pitch = width, no
            // padding), then interleaved UV plane (pitch = width,
            // height = h/2). Chroma readback signature `[93, 6a, 93,
            // 6a, …]` confirmed the UV plane is interleaved (treating
            // it as planar U + planar V produced two muted-grey halves
            // that looked correct as samples but rendered as
            // near-greyscale through BT.601).
            let y_pitch = w as usize;
            let uv_pitch = w as usize;
            let y_len = y_pitch * h as usize;
            let uv_len = uv_pitch * (h as usize / 2);
            let base = info.pData;
            let y = core::slice::from_raw_parts(base, y_len);
            let uv = core::slice::from_raw_parts(base.add(y_len), uv_len);
            Some(YuvFrame {
                width: w,
                height: h,
                timestamp_us: info.timeStamp,
                y,
                y_pitch,
                uv,
                uv_pitch,
            })
        }
        #[cfg(not(target_os = "vita"))]
        {
            None
        }
    }

    /// Pull the next chunk of decoded audio samples, if any.
    pub fn next_audio_samples(&mut self) -> Option<AudioChunk<'_>> {
        #[cfg(target_os = "vita")]
        unsafe {
            let mut info: sce::SceAvPlayerFrameInfo = core::mem::zeroed();
            let got = sce::sceAvPlayerGetAudioData(self.handle, &mut info);
            if got == 0 {
                return None;
            }
            let audio = info.details.audio;
            let len_bytes = audio.size as usize;
            let len_samples = len_bytes / core::mem::size_of::<i16>();
            let samples = core::slice::from_raw_parts(info.pData as *const i16, len_samples);
            Some(AudioChunk {
                samples,
                sample_rate: audio.sampleRate,
                channel_count: audio.channelCount,
            })
        }
        #[cfg(not(target_os = "vita"))]
        {
            None
        }
    }

    pub fn pause(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            sce::sceAvPlayerPause(self.handle);
        }
        self.state = PlayerState::Paused;
    }

    pub fn resume(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            sce::sceAvPlayerResume(self.handle);
        }
        self.state = PlayerState::Playing;
    }

    pub fn jump_to_time_us(&mut self, t: u64) {
        #[cfg(target_os = "vita")]
        unsafe {
            sce::sceAvPlayerJumpToTime(self.handle, t);
        }
    }

    pub fn current_time_us(&self) -> u64 {
        #[cfg(target_os = "vita")]
        unsafe {
            sce::sceAvPlayerCurrentTime(self.handle)
        }
        #[cfg(not(target_os = "vita"))]
        {
            0
        }
    }

    pub fn state(&self) -> PlayerState {
        self.state
    }

    pub fn video_dimensions(&self) -> (u32, u32) {
        (self.video_w, self.video_h)
    }

    /// Called when `getVideoData` returned no frame. If the player
    /// has already produced at least one frame and is no longer
    /// active, mark EOF. (Pre-warmup `isActive == 0` is not EOF — it
    /// just means the demuxer hasn't reported a stream yet.)
    #[cfg(target_os = "vita")]
    fn tick_eof(&mut self) {
        if !self.seen_frame || self.state == PlayerState::Eof {
            return;
        }
        let active = unsafe { sce::sceAvPlayerIsActive(self.handle) };
        if active == 0 {
            self.state = PlayerState::Eof;
        }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            sce::sceAvPlayerStop(self.handle);
            sce::sceAvPlayerClose(self.handle);
            // Reclaim the FileState. The file-replacement `close`
            // callback already called sceIoClose on shutdown — but
            // close it again just in case (sceIoClose on -1 is a
            // no-op error).
            if !self.file_state.is_null() {
                let state = Box::from_raw(self.file_state);
                if state.fd >= 0 {
                    sce::sceIoClose(state.fd);
                }
            }
        }
    }
}

// ─── Audio out ─────────────────────────────────────────────────────────

/// Stereo 16-bit PCM output port. Lazily opened the first time
/// `write` is called so the sample rate matches the source.
pub struct AudioOut {
    #[cfg(target_os = "vita")]
    port: i32,
    /// Sample rate / channel count / granularity drive the
    /// `sceAudioOutOpenPort` parameters. Only read inside the
    /// `target_os = "vita"` branch of `write`.
    #[allow(dead_code)]
    sample_rate: u32,
    #[allow(dead_code)]
    channel_count: u16,
    #[allow(dead_code)]
    granularity: u32,
}

impl AudioOut {
    /// Construct without opening the audio port. The first `write`
    /// call opens it with the chunk's sample rate / granularity.
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "vita")]
            port: -1,
            sample_rate: 0,
            channel_count: 0,
            granularity: 0,
        }
    }

    /// Push samples to the audio chip. Blocks (briefly) when the
    /// output queue is full; this naturally rate-limits playback to
    /// wall-clock. Re-opens the port if the chunk's
    /// rate/granularity drift from the previously-opened port.
    pub fn write(&mut self, chunk: &AudioChunk<'_>) -> Result<(), VideoError> {
        if chunk.samples.is_empty() {
            return Ok(());
        }
        let frame_count = chunk.samples.len() as u32 / chunk.channel_count.max(1) as u32;
        #[cfg(target_os = "vita")]
        unsafe {
            if self.port < 0
                || self.sample_rate != chunk.sample_rate
                || self.channel_count != chunk.channel_count
                || self.granularity != frame_count
            {
                if self.port >= 0 {
                    sce::sceAudioOutReleasePort(self.port);
                    self.port = -1;
                }
                let mode = if chunk.channel_count >= 2 {
                    sce::SCE_AUDIO_OUT_MODE_STEREO
                } else {
                    sce::SCE_AUDIO_OUT_MODE_MONO
                };
                let p = sce::sceAudioOutOpenPort(
                    sce::SCE_AUDIO_OUT_PORT_TYPE_MAIN,
                    frame_count as i32,
                    chunk.sample_rate as i32,
                    mode,
                );
                if p < 0 {
                    return Err(VideoError::AudioOpenFailed(p));
                }
                self.port = p;
                self.sample_rate = chunk.sample_rate;
                self.channel_count = chunk.channel_count;
                self.granularity = frame_count;
            }
            sce::sceAudioOutOutput(
                self.port,
                chunk.samples.as_ptr() as *const _,
            );
        }
        #[cfg(not(target_os = "vita"))]
        {
            let _ = (chunk, frame_count);
        }
        Ok(())
    }
}

impl Default for AudioOut {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        #[cfg(target_os = "vita")]
        unsafe {
            if self.port >= 0 {
                sce::sceAudioOutReleasePort(self.port);
            }
        }
    }
}

// ─── Allocator shims ───────────────────────────────────────────────────
//
// sceAvPlayer expects four `extern "C" fn` callbacks. We route through
// Rust's global allocator. Each allocation prepends a 16-byte header
// holding `size` + `align` so `deallocate` can reconstruct the Layout
// (the Vita library doesn't pass these back to us).

#[cfg(target_os = "vita")]
const HEADER: usize = 16;

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_alloc(
    _user: *mut core::ffi::c_void,
    alignment: u32,
    size: u32,
) -> *mut core::ffi::c_void {
    static ALLOC_COUNT: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0);
    let c = ALLOC_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if c <= 5 || c % 50 == 0 {
        bsky_log::log!("video: av_alloc #{c} align={alignment} size={size}");
    }
    let align = (alignment as usize).max(16);
    let total = size as usize + HEADER;
    let layout = match Layout::from_size_align(total, align) {
        Ok(l) => l,
        Err(_) => return core::ptr::null_mut(),
    };
    let raw = alloc(layout);
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    // Stash size + align in the first 16 bytes so av_free can reconstruct
    // the Layout. (size: u32, align: u32, pad: u64.)
    let header = raw as *mut u32;
    header.write(total as u32);
    header.add(1).write(align as u32);
    raw.add(HEADER) as *mut core::ffi::c_void
}

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_free(_user: *mut core::ffi::c_void, ptr: *mut core::ffi::c_void) {
    if ptr.is_null() {
        return;
    }
    let raw = (ptr as *mut u8).sub(HEADER);
    let header = raw as *mut u32;
    let total = header.read() as usize;
    let align = header.add(1).read() as usize;
    let layout = match Layout::from_size_align(total, align) {
        Ok(l) => l,
        Err(_) => return,
    };
    dealloc(raw, layout);
}

// ─── GPU-mapped texture allocator ────────────────────────────────────
//
// libsceAvPlayer's H.264 hardware decoder writes decoded YUV frames
// via DMA into buffers it requested from `allocateTexture`. Those
// buffers must be:
//   1. Allocated from a memblock the kernel knows about
//      (`sceKernelAllocMemBlock`).
//   2. Mapped into the GPU's virtual address space
//      (`sceGxmMapMemory`).
//   3. Aligned to the alignment the decoder asks for — observed
//      values up to 1 MB. We pass `HAS_ALIGNMENT` via opt so the
//      memblock base lands on the right boundary.
//
// Regular Rust heap allocations live in CPU-only memory the GPU MMU
// can't see — handing them to the decoder was the silent-failure
// path we hit before this fix landed.
//
// Because the user pointer must equal the memblock base (so the
// decoder sees a properly-aligned address), we can't stash a header
// inside the block. Instead, a side-table maps `base → UID` so
// `av_free_texture` can find the UID at free time.

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_alloc_texture(
    _user: *mut core::ffi::c_void,
    alignment: u32,
    size: u32,
) -> *mut core::ffi::c_void {
    static TEX_COUNT: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0);
    let c = TEX_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    bsky_log::log!("video: av_alloc_texture #{c} align={alignment} size={size}");

    // Memblocks default to 4 KB-aligned bases. The H.264 decoder
    // can request 1 MB alignment for its YUV ring buffer — we have
    // to ask the kernel for that alignment explicitly via the opt
    // struct (`HAS_ALIGNMENT` attr). Since the user pointer must
    // equal the memblock base (no header offset) for the decoder
    // to see an aligned address, store the UID in a side-table
    // keyed by base address.
    // H.264 hardware decoder needs physically-contiguous memory —
    // virtual-only mappings like USER_RW aren't sufficient, the
    // decoder DMAs from physical addresses. PHYCONT memblocks
    // already give 1 MB-aligned bases naturally; we round size up
    // to 1 MB and pass NULL opt (matching vitaGL's video sample —
    // passing HAS_ALIGNMENT via opt is what was returning
    // INVALID_ARGUMENT 0x80020005).
    const PHYCONT_GRANULE: usize = 1024 * 1024;
    let _ = alignment; // PHYCONT base is always >= 1 MB-aligned.
    let total = (size as usize + PHYCONT_GRANULE - 1) & !(PHYCONT_GRANULE - 1);

    let name = b"avp_tex\0".as_ptr() as *const core::ffi::c_char;
    let uid = sce::sceKernelAllocMemBlock(
        name,
        sce::SCE_KERNEL_MEMBLOCK_TYPE_USER_MAIN_PHYCONT_NC_RW,
        total as u32,
        core::ptr::null_mut(),
    );
    if uid < 0 {
        bsky_log::log!("video: sceKernelAllocMemBlock failed: {uid:#x}");
        return core::ptr::null_mut();
    }
    let mut base: *mut core::ffi::c_void = core::ptr::null_mut();
    let r = sce::sceKernelGetMemBlockBase(uid, &mut base);
    if r < 0 || base.is_null() {
        bsky_log::log!("video: sceKernelGetMemBlockBase failed: {r:#x}");
        sce::sceKernelFreeMemBlock(uid);
        return core::ptr::null_mut();
    }
    let map_r = sce::sceGxmMapMemory(
        base,
        total as u32,
        sce::SCE_GXM_MEMORY_ATTRIB_RW,
    );
    if map_r < 0 {
        bsky_log::log!("video: sceGxmMapMemory failed: {map_r:#x}");
        sce::sceKernelFreeMemBlock(uid);
        return core::ptr::null_mut();
    }
    bsky_log::log!("video: av_alloc_texture OK base={base:p} total={total}");
    base
}

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_free_texture(
    _user: *mut core::ffi::c_void,
    ptr: *mut core::ffi::c_void,
) {
    if ptr.is_null() {
        return;
    }
    // Look up the memblock UID by address — saves us a side-table.
    let uid = sce::sceKernelFindMemBlockByAddr(ptr, 0);
    if uid < 0 {
        bsky_log::log!("video: av_free_texture FindMemBlockByAddr failed: {uid:#x}");
        return;
    }
    sce::sceGxmUnmapMemory(ptr);
    sce::sceKernelFreeMemBlock(uid);
}

// ─── Event callback ──────────────────────────────────────────────────
//
// libsceAvPlayer dispatches state changes + warnings here. Common IDs
// observed in other Vita media homebrew (header isn't fully exposed in
// vitasdk-sys 0.3.3, so we decode by hand):
//
//   1   STATE_STOP
//   2   STATE_READY
//   3   STATE_PLAY
//   4   STATE_PAUSE
//   5   STATE_BUFFERING
//   16  TIMED_TEXT_DELIVERY
//   32  WARNING_ID  (decoder rejected, file unsupported, …)
//   48  ENCRYPTION
//   256 DRM_ERROR
//
// Source IDs identify the stream (0=video, 1=audio, etc).

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_event(
    _p: *mut core::ffi::c_void,
    event_id: i32,
    source_id: i32,
    _event_data: *mut core::ffi::c_void,
) {
    bsky_log::log!("video: av_event id={event_id} src={source_id}");
}

// ─── File replacement callbacks ──────────────────────────────────────
//
// sceAvPlayer's default file-I/O path doesn't grok the Vita's
// `ux0:` URIs (verified empirically — addSource succeeds but
// isActive never flips true with default callbacks). Wrap sceIo
// directly. Each callback receives our `FileState` pointer via
// the `objectPointer` field of `SceAvPlayerFileReplacement`.

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_file_open(
    p: *mut core::ffi::c_void,
    filename: *const core::ffi::c_char,
) -> core::ffi::c_int {
    let state = &mut *(p as *mut FileState);
    if state.fd >= 0 {
        // Already open from a previous AddSource — close + reopen.
        sce::sceIoClose(state.fd);
        state.fd = -1;
    }
    let fd = sce::sceIoOpen(filename, sce::SCE_O_RDONLY as i32, 0);
    if fd < 0 {
        bsky_log::log!("video: av_file_open sceIoOpen failed: {fd:#x}");
        return fd;
    }
    let size = sce::sceIoLseek(fd, 0, sce::SCE_SEEK_END as i32);
    sce::sceIoLseek(fd, 0, sce::SCE_SEEK_SET as i32);
    state.fd = fd;
    state.size = size as u64;
    bsky_log::log!("video: av_file_open fd={fd} size={size}");
    0
}

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_file_close(p: *mut core::ffi::c_void) -> core::ffi::c_int {
    let state = &mut *(p as *mut FileState);
    bsky_log::log!("video: av_file_close fd={}", state.fd);
    let r = if state.fd >= 0 {
        sce::sceIoClose(state.fd)
    } else {
        0
    };
    state.fd = -1;
    r
}

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_file_read(
    p: *mut core::ffi::c_void,
    buffer: *mut u8,
    position: u64,
    length: u32,
) -> core::ffi::c_int {
    let state = &*(p as *const FileState);
    if state.fd < 0 {
        bsky_log::log!("video: av_file_read with closed fd");
        return -1;
    }
    // sceAvPlayer reads concurrently from its demuxer + decoder
    // threads. Serialize the lseek + read pair so concurrent
    // callers don't clobber each other's seek.
    let _g = state
        .read_lock
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let off = sce::sceIoLseek(state.fd, position as i64, sce::SCE_SEEK_SET as i32);
    if off < 0 {
        bsky_log::log!("video: av_file_read lseek failed: {off:#x}");
        return -1;
    }
    let n = sce::sceIoRead(state.fd, buffer as *mut _, length);
    static READ_COUNT: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0);
    let c = READ_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if c <= 8 || c % 50 == 0 {
        bsky_log::log!("video: av_file_read #{c} pos={position} len={length} -> {n}");
    }
    n
}

#[cfg(target_os = "vita")]
unsafe extern "C" fn av_file_size(p: *mut core::ffi::c_void) -> u64 {
    let state = &*(p as *const FileState);
    bsky_log::log!("video: av_file_size -> {}", state.size);
    state.size
}

//! sceImeDialog wrapper.
//!
//! Exposes a state-machine API that fits a frame-driven render loop:
//!
//! ```text
//!   Closed ─open()─▶ Running ─poll()─▶ Finished(String) ┐
//!                            │                          │
//!                            ├──────▶ Cancelled         ├─close()─▶ Closed
//!                            │                          │
//!                            └──────▶ Aborted           ┘
//! ```
//!
//! Each frame the render loop:
//!   1. calls `ime.poll()` to advance the state
//!   2. if `ime.is_active()`, calls `frame.pump_ime()` between draws and
//!      the buffer swap so the IME panel keeps painting on top of our scene
//!
//! ## Buffer ownership
//!
//! `sceImeDialogInit` keeps pointers to our title/initial/input buffers
//! across the entire dialog lifetime. We hold them as `Box<[u16; N]>` so
//! they're stable in memory and Drop-cleaned with the `Ime`.
//!
//! ## UTF-16 ↔ UTF-8
//!
//! Vita's IME exchanges text as `u16` (UTF-16, LE in practice). We use
//! `str::encode_utf16` / `char::decode_utf16` from std — no custom
//! transcoder. Lone surrogates round-trip to U+FFFD.

#![allow(unused)]

#[cfg(target_os = "vita")]
use vitasdk_sys as sce;

/// Hard-coded SDK version field — the IME dialog uses it to pick a
/// rendering style. Any reasonable value works; this is what most Vita
/// homebrew shipping today reports.
#[cfg(target_os = "vita")]
const SDK_VERSION: u32 = 0x03570011;

/// Title buffer is small — enough for a short prompt like "Bluesky handle".
const TITLE_BUF: usize = 256;

/// Input/initial buffer — 1024 u16's = up to 1024 BMP chars or 512 surrogate
/// pairs. Bluesky handles top out at ~64 chars; app passwords are ~19 chars.
/// Posts can be 300 graphemes / ~3000 bytes UTF-8 → at most 3000 u16 (rare).
const INPUT_BUF: usize = 1024;

/// Input filtering / on-screen-keyboard layout hint.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ImeMode {
    Default,
    BasicLatin,
    Number,
    ExtendedNumber,
    Url,
    Mail,
}

/// How the typed text is rendered in the IME's input row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TextBoxMode {
    Default,
    Password,
    WithClear,
}

#[derive(Debug)]
pub enum ImeError {
    AlreadyOpen,
    InitFailed(i32),
    NotOnVita,
}

impl core::fmt::Display for ImeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ImeError::AlreadyOpen => write!(f, "IME is already open"),
            ImeError::InitFailed(code) => write!(f, "sceImeDialogInit failed: {code:#x}"),
            ImeError::NotOnVita => write!(f, "IME is only available on the Vita target"),
        }
    }
}

impl core::error::Error for ImeError {}

/// IME state. After `poll()` returns `Finished`/`Cancelled`/`Aborted`, the
/// dialog is dismissed; calling `close()` drains the result and resets to
/// `Closed`.
#[derive(Debug, Clone)]
pub enum ImeState {
    Closed,
    Running,
    Finished(String),
    Cancelled,
    Aborted,
}

/// Owns the per-instance UTF-16 buffers and the dialog state machine.
/// One Ime instance per app is enough — the system only allows one common
/// dialog active at a time anyway.
pub struct Ime {
    state: ImeState,
    title_buf: Box<[u16; TITLE_BUF]>,
    initial_buf: Box<[u16; INPUT_BUF]>,
    input_buf: Box<[u16; INPUT_BUF]>,
}

impl Default for Ime {
    fn default() -> Self {
        Self::new()
    }
}

impl Ime {
    pub fn new() -> Self {
        #[cfg(target_os = "vita")]
        unsafe {
            // (1) IME sysmodule must be loaded before sceImeDialogInit.
            // SCE_SYSMODULE_IME = 17. Idempotent.
            sce::sceSysmoduleLoadModule(17);

            // (2) The common-dialog system needs an initial config or
            // sceImeDialogInit succeeds-but-the-dialog-doesn't-render.
            // Mirrors the static-inline `sceCommonDialogConfigParamInit`
            // from psp2/common_dialog.h: sentinel MAX_VALUE for language
            // and enter-button means "use system pref."
            let mut config: sce::SceCommonDialogConfigParam = core::mem::zeroed();
            config.sdkVersion = SDK_VERSION;
            config.language = sce::SCE_SYSTEM_PARAM_LANG_MAX_VALUE;
            config.enterButtonAssign = sce::SCE_SYSTEM_PARAM_ENTER_BUTTON_MAX_VALUE;
            sce::sceCommonDialogSetConfigParam(&config);
        }
        Self {
            state: ImeState::Closed,
            title_buf: Box::new([0; TITLE_BUF]),
            initial_buf: Box::new([0; INPUT_BUF]),
            input_buf: Box::new([0; INPUT_BUF]),
        }
    }

    /// Is a dialog currently up (Running or with an unread result)?
    pub fn is_active(&self) -> bool {
        !matches!(self.state, ImeState::Closed)
    }

    /// Open the IME. `max_len` is in u16 codeunits, capped at the internal
    /// buffer size. Returns `AlreadyOpen` if a dialog is up.
    pub fn open(
        &mut self,
        title: &str,
        mode: ImeMode,
        textbox_mode: TextBoxMode,
        max_len: u32,
        initial_text: &str,
    ) -> Result<(), ImeError> {
        if self.is_active() {
            return Err(ImeError::AlreadyOpen);
        }

        encode_utf16_into(title, &mut *self.title_buf);
        encode_utf16_into(initial_text, &mut *self.initial_buf);
        self.input_buf.fill(0);

        #[cfg(target_os = "vita")]
        {
            let mut param: sce::SceImeDialogParam = unsafe { core::mem::zeroed() };
            param.sdkVersion = SDK_VERSION;
            param.inputMethod = 0;
            param.supportedLanguages = 0; // 0 = all
            param.languagesForced = 0;
            param.type_ = match mode {
                ImeMode::Default => 0,
                ImeMode::BasicLatin => 1,
                ImeMode::Number => 2,
                ImeMode::ExtendedNumber => 3,
                ImeMode::Url => 4,
                ImeMode::Mail => 5,
            };
            param.option = 0;
            // DIALOG_MODE_WITH_CANCEL = 1 — show a Cancel button alongside
            // OK so users can back out.
            param.dialogMode = 1;
            param.textBoxMode = match textbox_mode {
                TextBoxMode::Default => 0,
                TextBoxMode::Password => 1,
                TextBoxMode::WithClear => 2,
            };
            param.title = self.title_buf.as_ptr();
            param.maxTextLength = max_len.min(INPUT_BUF as u32 - 1);
            param.initialText = self.initial_buf.as_mut_ptr();
            param.inputTextBuffer = self.input_buf.as_mut_ptr();
            param.enterLabel = 0;

            // SceCommonDialogParam carries a `magic` field the kernel
            // validates on init. The static-inline `_sceCommonDialogSetMagicNumber`
            // in psp2/common_dialog.h sets:
            //   param->magic = 0xC0D1A109 + (u32-cast of &param)
            // so the magic is per-instance (won't accept a copy/replay).
            // Without this, sceImeDialogInit returns 0x80020403
            // (SCE_COMMON_DIALOG_ERROR_INVALID_ARGUMENT).
            let common_ptr =
                (&mut param.commonParam) as *mut sce::SceCommonDialogParam;
            param.commonParam.magic =
                0xC0D1A109u32.wrapping_add(common_ptr as usize as u32);

            let r = unsafe { sce::sceImeDialogInit(&param) };
            if r < 0 {
                return Err(ImeError::InitFailed(r));
            }
            self.state = ImeState::Running;
            Ok(())
        }

        #[cfg(not(target_os = "vita"))]
        {
            let _ = (mode, textbox_mode, max_len);
            Err(ImeError::NotOnVita)
        }
    }

    /// Advance the state machine by one frame. Returns the current state.
    /// While `Running`, this calls `sceImeDialogGetStatus`. On the
    /// `FINISHED` transition, it reads the result and calls `Term`.
    pub fn poll(&mut self) -> ImeState {
        #[cfg(target_os = "vita")]
        if matches!(self.state, ImeState::Running) {
            // SCE_COMMON_DIALOG_STATUS_FINISHED = 2
            let status = unsafe { sce::sceImeDialogGetStatus() };
            if status == 2 {
                let mut result: sce::SceImeDialogResult = unsafe { core::mem::zeroed() };
                unsafe { sce::sceImeDialogGetResult(&mut result) };
                // SCE_COMMON_DIALOG_RESULT_ABORTED = 2
                let new_state = if result.result == 2 {
                    ImeState::Aborted
                } else if result.button == 2 {
                    // SCE_IME_DIALOG_BUTTON_ENTER = 2
                    ImeState::Finished(decode_utf16(&self.input_buf[..]))
                } else {
                    ImeState::Cancelled
                };
                unsafe { sce::sceImeDialogTerm() };
                self.state = new_state;
            }
        }
        self.state.clone()
    }

    /// Drain the result and reset to Closed. If the IME is still Running,
    /// this aborts it first.
    pub fn close(&mut self) -> Option<String> {
        #[cfg(target_os = "vita")]
        if matches!(self.state, ImeState::Running) {
            unsafe {
                sce::sceImeDialogAbort();
                sce::sceImeDialogTerm();
            }
        }
        let prev = core::mem::replace(&mut self.state, ImeState::Closed);
        match prev {
            ImeState::Finished(s) => Some(s),
            _ => None,
        }
    }
}

/// UTF-8 `s` → UTF-16 in `dest`, NUL-terminated. Truncates if `s` exceeds
/// `dest.len() - 1` u16 units (one slot reserved for the terminator).
fn encode_utf16_into(s: &str, dest: &mut [u16]) {
    let cap = dest.len();
    if cap == 0 {
        return;
    }
    let mut i = 0;
    for code_unit in s.encode_utf16() {
        if i >= cap - 1 {
            break;
        }
        dest[i] = code_unit;
        i += 1;
    }
    // NUL-terminate + zero the tail for cleanliness.
    for slot in &mut dest[i..] {
        *slot = 0;
    }
}

/// UTF-16 → String, stopping at the first NUL u16 (or end-of-slice).
/// Lone surrogates become U+FFFD.
fn decode_utf16(src: &[u16]) -> String {
    let end = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    char::decode_utf16(src[..end].iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_ascii_roundtrip() {
        let mut buf = [0u16; 32];
        encode_utf16_into("hello", &mut buf);
        assert_eq!(buf[5], 0, "NUL terminator");
        assert_eq!(decode_utf16(&buf), "hello");
    }

    #[test]
    fn utf16_emoji_surrogate_pair() {
        // 🦀 = U+1F980, encoded as a UTF-16 surrogate pair.
        let mut buf = [0u16; 32];
        encode_utf16_into("a🦀b", &mut buf);
        assert_eq!(decode_utf16(&buf), "a🦀b");
    }

    #[test]
    fn utf16_japanese_roundtrip() {
        let mut buf = [0u16; 32];
        encode_utf16_into("こんにちは", &mut buf);
        assert_eq!(decode_utf16(&buf), "こんにちは");
    }

    #[test]
    fn truncates_when_input_exceeds_buffer() {
        // 4-slot buffer holds 3 chars + NUL.
        let mut buf = [0u16; 4];
        encode_utf16_into("abcdef", &mut buf);
        assert_eq!(buf[3], 0);
        assert_eq!(decode_utf16(&buf), "abc");
    }

    #[test]
    fn lone_high_surrogate_becomes_replacement() {
        let bad = [0xD800u16, b'!' as u16, 0];
        assert_eq!(decode_utf16(&bad), "\u{FFFD}!");
    }

    #[test]
    fn ime_state_lifecycle_on_host_returns_not_on_vita() {
        let mut ime = Ime::new();
        assert!(!ime.is_active());
        match ime.open("title", ImeMode::BasicLatin, TextBoxMode::Default, 64, "") {
            Err(ImeError::NotOnVita) => {}
            other => panic!("expected NotOnVita, got {other:?}"),
        }
        assert!(!ime.is_active());
        assert!(matches!(ime.poll(), ImeState::Closed));
        assert!(ime.close().is_none());
    }
}

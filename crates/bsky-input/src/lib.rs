//! Input wrappers: sceCtrl (buttons + analog sticks) and sceTouch (front
//! panel only — rear panel goes unused for Phase 2).
//!
//! Designed for a frame-driven render loop: `Pad::poll()` and `Touch::poll()`
//! return per-frame snapshots that include both the current state and edge
//! triggers (just_pressed / just_released) computed against the previous
//! frame.
//!
//! Coordinate normalization:
//! - Analog sticks: raw `u8` 0..=255 reported by `sceCtrlPeekBufferPositive2`
//!   centered to `i8` (~-128..=127). Deadzone handling is left to callers.
//! - Touch: raw ADC coords (front panel ~0..1920 × 0..1088) converted to
//!   display pixels (0..960 × 0..544) via the panel-info factors fetched
//!   once at `Touch::init`.

#![allow(unused)]

#[cfg(target_os = "vita")]
use vitasdk_sys as sce;

/// Bitmask flags for buttons currently held. Combined freely with `&` for
/// "is this button pressed" checks.
pub mod buttons {
    /// Cross (×) — bottom face button. Default "enter" in western locales.
    pub const CROSS: u32 = 1 << 14;
    /// Circle (○) — right face button. Default "enter" in JP locale.
    pub const CIRCLE: u32 = 1 << 13;
    /// Triangle (△) — top face button.
    pub const TRIANGLE: u32 = 1 << 12;
    /// Square (□) — left face button.
    pub const SQUARE: u32 = 1 << 15;
    pub const SELECT: u32 = 1 << 0;
    pub const START: u32 = 1 << 3;
    pub const UP: u32 = 1 << 4;
    pub const RIGHT: u32 = 1 << 5;
    pub const DOWN: u32 = 1 << 6;
    pub const LEFT: u32 = 1 << 7;
    pub const L1: u32 = 1 << 10;
    pub const R1: u32 = 1 << 11;
}

/// Per-frame snapshot of pad state + edge triggers vs the previous frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct PadFrame {
    /// Bitmask of currently held buttons.
    pub current: u32,
    /// Bitmask from the previous frame, for edge detection.
    pub previous: u32,
    /// Left analog stick, ~-128..=127 (centered from raw u8).
    pub left_stick: (i8, i8),
    /// Right analog stick, ~-128..=127.
    pub right_stick: (i8, i8),
}

impl PadFrame {
    /// Any of the buttons in `mask` are currently held.
    pub fn pressed(&self, mask: u32) -> bool {
        self.current & mask != 0
    }
    /// A button transition from up→down this frame.
    pub fn just_pressed(&self, mask: u32) -> bool {
        (self.current & mask) != 0 && (self.previous & mask) == 0
    }
    /// A button transition from down→up this frame.
    pub fn just_released(&self, mask: u32) -> bool {
        (self.current & mask) == 0 && (self.previous & mask) != 0
    }
}

/// Pad sampler. Construct once at app startup; call [`Pad::poll`] every frame.
pub struct Pad {
    last: u32,
}

impl Default for Pad {
    fn default() -> Self {
        Self::init()
    }
}

impl Pad {
    pub fn init() -> Self {
        #[cfg(target_os = "vita")]
        unsafe {
            // SCE_CTRL_MODE_ANALOG = 1: enable analog stick reads in
            // sceCtrlPeekBufferPositive2's SceCtrlData. Default is digital-only.
            sce::sceCtrlSetSamplingMode(1);
        }
        Self { last: 0 }
    }

    pub fn poll(&mut self) -> PadFrame {
        #[cfg(target_os = "vita")]
        {
            let mut data: sce::SceCtrlData = unsafe { core::mem::zeroed() };
            unsafe {
                sce::sceCtrlPeekBufferPositive2(0, &mut data, 1);
            }
            let frame = PadFrame {
                current: data.buttons,
                previous: self.last,
                left_stick: (analog(data.lx), analog(data.ly)),
                right_stick: (analog(data.rx), analog(data.ry)),
            };
            self.last = data.buttons;
            frame
        }
        #[cfg(not(target_os = "vita"))]
        {
            PadFrame::default()
        }
    }
}

/// Center a raw u8 (0..=255 with 128 as neutral) into i8 (~-128..=127).
fn analog(raw: u8) -> i8 {
    (raw as i16 - 128).clamp(-128, 127) as i8
}

/// One touch contact point with display-pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct TouchPoint {
    /// Stable touch id within this contact's lifetime (a finger keeps the
    /// same id from down to up, even if the finger drags around).
    pub id: u8,
    /// Display-pixel x, 0..960.
    pub x: i32,
    /// Display-pixel y, 0..544.
    pub y: i32,
    /// Raw force (0..128 typical). Vita's touchscreen doesn't really support
    /// pressure but the field exists.
    pub force: u8,
}

/// Up to 6 simultaneous touches on the front panel.
#[derive(Debug, Clone, Default)]
pub struct TouchFrame {
    pub points: Vec<TouchPoint>,
}

/// Touch sampler for the front panel. Reads `sceTouchGetPanelInfo` once at
/// startup to cache the coordinate-conversion factors.
pub struct Touch {
    #[cfg(target_os = "vita")]
    panel: TouchPanel,
}

#[cfg(target_os = "vita")]
struct TouchPanel {
    min_disp_x: i16,
    min_disp_y: i16,
    span_disp_x: i32,
    span_disp_y: i32,
}

impl Default for Touch {
    fn default() -> Self {
        Self::init()
    }
}

impl Touch {
    pub fn init() -> Self {
        #[cfg(target_os = "vita")]
        unsafe {
            // Front panel only; rear panel left at default (off for Phase 2).
            // SCE_TOUCH_PORT_FRONT = 0, SCE_TOUCH_SAMPLING_STATE_START = 1.
            sce::sceTouchSetSamplingState(0, 1);

            let mut info: sce::SceTouchPanelInfo = core::mem::zeroed();
            sce::sceTouchGetPanelInfo(0, &mut info);

            let span_x =
                (info.maxDispX as i32 - info.minDispX as i32).max(1);
            let span_y =
                (info.maxDispY as i32 - info.minDispY as i32).max(1);

            Self {
                panel: TouchPanel {
                    min_disp_x: info.minDispX,
                    min_disp_y: info.minDispY,
                    span_disp_x: span_x,
                    span_disp_y: span_y,
                },
            }
        }
        #[cfg(not(target_os = "vita"))]
        Self {}
    }

    pub fn poll(&self) -> TouchFrame {
        #[cfg(target_os = "vita")]
        {
            let mut data: sce::SceTouchData = unsafe { core::mem::zeroed() };
            unsafe {
                sce::sceTouchPeek(0, &mut data, 1);
            }
            let n = (data.reportNum as usize).min(data.report.len());
            let mut points = Vec::with_capacity(n);
            for i in 0..n {
                let r = &data.report[i];
                let x = (r.x as i32 - self.panel.min_disp_x as i32) * 960
                    / self.panel.span_disp_x;
                let y = (r.y as i32 - self.panel.min_disp_y as i32) * 544
                    / self.panel.span_disp_y;
                points.push(TouchPoint {
                    id: r.id,
                    x,
                    y,
                    force: r.force,
                });
            }
            TouchFrame { points }
        }
        #[cfg(not(target_os = "vita"))]
        {
            TouchFrame::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analog_centers_on_128() {
        assert_eq!(analog(128), 0);
        assert_eq!(analog(0), -128);
        assert_eq!(analog(255), 127);
    }

    #[test]
    fn pad_frame_edge_detection() {
        let pf = PadFrame {
            current: buttons::CROSS,
            previous: 0,
            ..Default::default()
        };
        assert!(pf.pressed(buttons::CROSS));
        assert!(pf.just_pressed(buttons::CROSS));
        assert!(!pf.just_released(buttons::CROSS));

        let pf = PadFrame {
            current: 0,
            previous: buttons::CROSS,
            ..Default::default()
        };
        assert!(!pf.pressed(buttons::CROSS));
        assert!(!pf.just_pressed(buttons::CROSS));
        assert!(pf.just_released(buttons::CROSS));
    }

    #[test]
    fn pad_frame_held_button_no_edges() {
        let pf = PadFrame {
            current: buttons::CIRCLE,
            previous: buttons::CIRCLE,
            ..Default::default()
        };
        assert!(pf.pressed(buttons::CIRCLE));
        assert!(!pf.just_pressed(buttons::CIRCLE));
        assert!(!pf.just_released(buttons::CIRCLE));
    }
}

//! [`Screen`] trait — what a top-level UI view implements.
//!
//! Two methods, called once per main-loop iteration:
//!
//! - [`Screen::frame`] — drawing + input. Returns a [`ScreenAction`]
//!   (consumed by main.rs after the frame is presented).
//! - [`Screen::after_present`] — called *after* the rendered frame has
//!   been swapped to the display. Override this to do blocking network
//!   work; the user sees the just-rendered frame (typically a "Loading…"
//!   or "Authenticating…" overlay) while the call is in flight.
//!
//! Together these let a screen respond synchronously to "I need to fetch
//! data" without freezing mid-paint:
//!
//! 1. Frame N: `frame()` renders "Loading…" and remains in `Pending` state.
//! 2. Frame N's Drop swaps the buffer — user sees the loading state.
//! 3. `after_present()` runs the blocking call, transitions state to
//!    `Loaded(data)`.
//! 4. Frame N+1: `frame()` renders the data.
//!
//! Phase 3+ may swap this for a worker-thread approach when the freeze
//! becomes unacceptable (timeline polling). For Phase 2.5 it's fine.

use bsky_ime::Ime;
use bsky_render::{Font, Frame};

use crate::widget::UiCtx;

pub trait Screen {
    /// Render this frame and update widget state. Return a transition
    /// action consumed by main.rs after the frame is presented.
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        ime: &mut Ime,
    ) -> ScreenAction;

    /// Called once after the just-drawn frame has been swapped to the
    /// display. Default no-op; screens override to do blocking work.
    fn after_present(&mut self) {}
}

/// Outcome of a `Screen::frame` call. Drives screen routing in main.rs.
pub enum ScreenAction {
    /// Stay on the current screen.
    None,
    /// Replace the current screen with this one. The old screen is
    /// dropped.
    Goto(Box<dyn Screen>),
}

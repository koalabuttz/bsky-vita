//! [`Screen`] trait — what a top-level UI view implements.
//!
//! Three methods, each called once per main-loop iteration in this order:
//!
//! - [`Screen::frame`] — drawing + input. Returns a [`ScreenAction`]
//!   (consumed by main.rs after the frame is presented). Screens that need
//!   network data dispatch a [`WorkRequest`] via the worker handle in
//!   `UiCtx` and remain in a "Pending" visual state until a response
//!   arrives.
//! - [`Screen::handle_worker_response`] — called for each [`WorkResponse`]
//!   the worker has produced since the last frame. Default no-op; screens
//!   that dispatched work override this to update their state.
//! - [`Screen::after_present`] — called *after* the rendered frame has
//!   been swapped to the display. Legacy hook for blocking work that
//!   happens before the worker exists (e.g. LoginScreen's resume / login).
//!
//! ### Why two paths (worker vs after_present)?
//!
//! `after_present` is the Phase 2.5 pattern: render "Loading…", swap, then
//! block. It works for one-shot calls but freezes the render loop. Phase
//! 3.1 introduces the worker pattern for the post-auth steady state where
//! freezes would be visible. LoginScreen still uses `after_present`
//! because it runs *before* the worker exists (the worker is spawned at
//! `ScreenAction::AuthComplete`, with the freshly-acquired AuthClient).

use std::sync::Arc;

use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_render::{Font, Frame};
use bsky_worker::WorkResponse;

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

    /// Update screen state from a worker response. Default no-op for
    /// screens that don't dispatch work (e.g. LoginScreen).
    fn handle_worker_response(&mut self, _resp: WorkResponse) {}

    /// Called once after the just-drawn frame has been swapped to the
    /// display. Default no-op; LoginScreen overrides for resume / login
    /// (pre-worker blocking work).
    fn after_present(&mut self) {}
}

/// Outcome of a `Screen::frame` call. Drives screen routing in main.rs.
pub enum ScreenAction {
    /// Stay on the current screen.
    None,
    /// Replace the current screen with this one. The old screen is
    /// dropped.
    Goto(Box<dyn Screen>),
    /// LoginScreen → ProfileScreen transition. The `client` is handed to
    /// main.rs so it can spawn the worker; the `next` screen already
    /// holds its own clone of the same Arc.
    AuthComplete {
        client: Arc<AuthClient>,
        next: Box<dyn Screen>,
    },
}

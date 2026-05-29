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

use crate::tabbar::TopLevel;
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

    /// `Some(level)` for top-level screens (Timeline, Profile-of-self,
    /// Notifications, Search) — these render the tab bar and respond to
    /// `SwitchTab` by being truncate-targets in the screen stack.
    /// `None` for pushed sub-screens (Compose, Thread, Profile-of-other).
    /// Default `None` so screens opt in.
    fn top_level(&self) -> Option<TopLevel> {
        None
    }
}

/// Outcome of a `Screen::frame` call. Drives screen routing in main.rs.
pub enum ScreenAction {
    /// Stay on the current screen.
    None,
    /// Push a sub-screen onto the navigation stack. CIRCLE on the new
    /// screen (if it handles CIRCLE) `Pop`s back here. Used for thread
    /// view, compose, profile-of-other.
    Push(Box<dyn Screen>),
    /// Pop the current screen off the stack, returning to the screen
    /// below. No-op if the current screen is the only one on the stack.
    /// Top-level screens typically don't emit this (CIRCLE on a
    /// top-level is a no-op).
    Pop,
    /// Tab-bar tap. main.rs walks the stack from the bottom; if a screen
    /// with `top_level() == Some(target)` exists, the stack is truncated
    /// to (and including) that screen. Otherwise main.rs constructs a
    /// fresh instance of the target's top-level screen and pushes it as
    /// the new root.
    SwitchTab(TopLevel),
    /// LoginScreen → ProfileScreen transition. The `client` is handed to
    /// main.rs so it can spawn the worker + retain a handle for
    /// constructing top-level screens later via `SwitchTab`. The `next`
    /// screen already holds its own clone of the same Arc.
    AuthComplete {
        client: Arc<AuthClient>,
        next: Box<dyn Screen>,
    },
    /// Log out: main.rs tears down the worker + auth client, deletes the
    /// on-disk session, and resets the stack to a fresh `LoginScreen`.
    Logout,
}

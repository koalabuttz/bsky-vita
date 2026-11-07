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
    fn response_owner(&self) -> Option<u64> { None }
    fn handle_owned_response(&mut self, _request_id: u64, response: WorkResponse) {
        self.handle_worker_response(response);
    }
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

    /// Primary, teachable controls for this screen, shown in the
    /// auto-hiding hint bar (drawn centrally by main.rs). Each entry is
    /// `(button label, action)`, e.g. `("L1", "Like")`. Curate to the
    /// non-obvious controls — scrolling and tab-bar nav are intuitive.
    /// Default: none, so the bar doesn't appear (used by screens that
    /// already render their own hints, like the video player).
    fn control_hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}

pub(crate) fn new_response_owner() -> u64 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    u64::from(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A covered screen still owns its reads; a removed screen's replies are dropped.
pub fn dispatch_worker_response(stack: &mut [Box<dyn Screen>], response: WorkResponse) {
    match response {
        WorkResponse::ForScreen { owner, request_id, response } => {
            if let Some(screen) = stack.iter_mut().find(|s| s.response_owner() == Some(owner)) {
                screen.handle_owned_response(request_id, *response);
            }
        }
        response => {
            if let Some(top) = stack.last_mut() { top.handle_worker_response(response); }
        }
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

#[cfg(test)]
mod routing_tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};
    struct Receiver { owner: u64, received: Rc<Cell<usize>> }
    impl Screen for Receiver {
        fn frame(&mut self, _: &mut Frame, _: &Font, _: &UiCtx, _: &mut Ime) -> ScreenAction { unreachable!() }
        fn response_owner(&self) -> Option<u64> { Some(self.owner) }
        fn handle_owned_response(&mut self, request_id: u64, _: WorkResponse) {
            assert_eq!(request_id, 9);
            self.received.set(self.received.get() + 1);
        }
    }
    fn response(owner: u64) -> WorkResponse {
        WorkResponse::ForScreen { owner, request_id: 9, response: Box::new(WorkResponse::PostCreated(Ok("test".into()))) }
    }
    #[test]
    fn covered_screen_receives_its_response_and_removed_owner_is_ignored() {
        let feed = Rc::new(Cell::new(0));
        let cover = Rc::new(Cell::new(0));
        let mut stack: Vec<Box<dyn Screen>> = vec![
            Box::new(Receiver { owner: 1, received: feed.clone() }),
            Box::new(Receiver { owner: 2, received: cover.clone() }),
        ];
        dispatch_worker_response(&mut stack, response(1));
        assert_eq!(feed.get(), 1);
        assert_eq!(cover.get(), 0);
        stack.remove(0);
        dispatch_worker_response(&mut stack, response(1));
        assert_eq!(cover.get(), 0);
    }
}

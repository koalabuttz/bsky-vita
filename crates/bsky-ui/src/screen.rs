//! [`Screen`] trait — what a top-level UI view implements.

use bsky_ime::Ime;
use bsky_render::{Font, Frame};

use crate::widget::UiCtx;

/// A full-screen UI view. The single `frame()` method does input
/// handling + drawing in one pass. Immediate-mode widgets hit-test as
/// they draw, so separating update from draw would force redundant
/// rect-passing.
///
/// Phase 2.5 will extend this to return a `ScreenAction` for routing
/// (e.g. `Goto(Box<dyn Screen>)`); for 2.4 there's only one screen so
/// there's nothing to return.
pub trait Screen {
    fn frame(&mut self, frame: &mut Frame, font: &Font, ctx: &UiCtx, ime: &mut Ime);
}

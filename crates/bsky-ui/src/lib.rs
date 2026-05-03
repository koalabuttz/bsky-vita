//! Immediate-mode UI for bsky-vita.
//!
//! ## Shape
//!
//! - `widget` — drawing+hit-test primitives (`Rect`, `Label`, `Button`,
//!   `TextField`). Each widget is a function that takes `&mut Frame`,
//!   the input `UiCtx`, and an `&mut <state>` for whatever it needs to
//!   remember across frames.
//! - `screen` — the [`Screen`] trait every full-screen view implements.
//!   A single `frame()` method does update + draw in one pass, which
//!   plays well with immediate-mode (the widget hit-tests *while*
//!   drawing, so we can't separate update from draw cleanly).
//! - `login` — the [`LoginScreen`].

pub mod login;
pub mod screen;
pub mod widget;

pub use login::LoginScreen;
pub use screen::Screen;
pub use widget::{ButtonState, FieldState, Rect, UiCtx};

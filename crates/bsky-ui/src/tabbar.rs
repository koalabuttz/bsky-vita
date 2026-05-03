//! Bottom tab bar — bsky-mobile-style top-level navigation.
//!
//! Persistent ~60 px sticky footer on every top-level screen
//! (TimelineScreen, ProfileScreen, NotificationsScreen, SearchScreen).
//! Tap a tab → emits a [`TopLevel`] which main.rs translates into a
//! `ScreenAction::SwitchTab` to truncate the stack to (or push a fresh
//! instance of) that tab's top-level screen. Active tab gets an
//! ACCENT-colored underline.
//!
//! Pushed sub-screens (Compose, Thread, ProfileScreen for an arbitrary
//! actor) do NOT render the tab bar — they're navigationally "deeper"
//! and CIRCLE pops them back to whatever top-level is below.

use bsky_render::{theme, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};

use crate::widget::{ButtonState, Rect, UiCtx};

/// Identifies one of the four top-level navigation destinations.
/// `top_level()` on the [`Screen`](crate::screen::Screen) trait returns
/// `Some(TopLevel)` for top-level screens and `None` for pushed
/// sub-screens.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TopLevel {
    Home,
    Search,
    Notifications,
    Profile,
}

impl TopLevel {
    fn label(self) -> &'static str {
        match self {
            TopLevel::Home => "Home",
            TopLevel::Search => "Search",
            TopLevel::Notifications => "Notifs",
            TopLevel::Profile => "Profile",
        }
    }

    fn all() -> [TopLevel; 4] {
        [
            TopLevel::Home,
            TopLevel::Search,
            TopLevel::Notifications,
            TopLevel::Profile,
        ]
    }
}

/// Visual + tap state for the bottom tab bar. Each top-level screen
/// owns one and `render`s it last in its `frame()` impl.
pub struct TabBar {
    pub active: TopLevel,
    home: ButtonState,
    search: ButtonState,
    notifications: ButtonState,
    profile: ButtonState,
}

impl TabBar {
    pub fn new(active: TopLevel) -> Self {
        Self {
            active,
            home: ButtonState::default(),
            search: ButtonState::default(),
            notifications: ButtonState::default(),
            profile: ButtonState::default(),
        }
    }

    /// Render the bar and return `Some(target)` if the user tapped a
    /// tab. The caller turns that into `ScreenAction::SwitchTab(target)`.
    /// Tapping the currently-active tab returns `None` (no-op).
    pub fn render(&mut self, frame: &mut Frame, font: &Font, ctx: &UiCtx) -> Option<TopLevel> {
        let h = TAB_BAR_HEIGHT;
        let y = SCREEN_HEIGHT - h;
        let w = SCREEN_WIDTH;

        // Bar background + 1 px top separator.
        frame.fill_rect(0.0, y as f32, w as f32, h as f32, theme::FIELD_BG);
        frame.fill_rect(0.0, y as f32, w as f32, 1.0, theme::TEXT_MUTED);

        let cell_w = w / 4;
        let mut clicked: Option<TopLevel> = None;

        for (i, tab) in TopLevel::all().iter().enumerate() {
            let x = (i as i32) * cell_w;
            let rect = Rect::new(x as f32, y as f32, cell_w as f32, h as f32);
            let state = match tab {
                TopLevel::Home => &mut self.home,
                TopLevel::Search => &mut self.search,
                TopLevel::Notifications => &mut self.notifications,
                TopLevel::Profile => &mut self.profile,
            };
            let pressed_now = ctx.touches.iter().any(|t| rect.contains(t.x, t.y));
            let just_clicked =
                state.pressed_last && !pressed_now && ctx.touches.is_empty();
            state.pressed_last = pressed_now;
            if just_clicked && *tab != self.active {
                clicked = Some(*tab);
            }

            // Label, vertically centered.
            let is_active = *tab == self.active;
            let color = if is_active {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_MUTED
            };
            let label = tab.label();
            let scale = 0.95;
            let (tw, th) = frame.measure_text(font, scale, label);
            let tx = x + (cell_w - tw) / 2;
            let ty = y + (h + th) / 2 - 6;
            frame.draw_text(font, tx, ty, color, scale, label);

            // Active indicator: 3 px accent bar across the top of the cell.
            if is_active {
                frame.fill_rect(
                    x as f32,
                    y as f32,
                    cell_w as f32,
                    3.0,
                    theme::ACCENT,
                );
            }
        }

        clicked
    }
}

/// Tab bar height in pixels. Top-level screens that render the tab bar
/// reserve this many pixels at the bottom of their content area.
pub const TAB_BAR_HEIGHT: i32 = 60;

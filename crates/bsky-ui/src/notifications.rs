//! Notifications screen — top-level.
//!
//! Reachable via the tab bar's Notifs tab. Lists recent notifications
//! (likes / reposts / follows / mentions / replies / quotes) with
//! reason-aware text. Tap a notification to navigate to context: a
//! post-engagement notification opens the relevant thread; a follow
//! notification opens that profile.
//!
//! Phase 4.6 MVP scope:
//! - Render: avatar (small) + text line per notification + relative
//!   timestamp; unread rows get a subtle ACCENT-tinted left bar.
//! - Selection model: d-pad up/down moves the focused row; CIRCLE on
//!   top-level is a no-op (tab-switch via tab bar); analog stick
//!   free-scrolls.
//! - Mark-seen: dispatch `WorkRequest::MarkSeen` once on screen entry.
//! - Pagination: cursor-based, like TimelineScreen.
//! - Tap: opens the relevant thread / profile.
//!
//! Out of scope (4.6.x or 4.x polish):
//! - System notification integration (`sceNotificationUtil`) — see
//!   `system_notifications_idea.md` memory.
//! - Filtering by reason (Bluesky's "Mentions" / "Likes" tabs).
//! - Real-time unread badge on the tab bar Notifs cell.

use std::collections::HashSet;
use std::sync::Arc;

use atrium_api::app::bsky::notification::list_notifications::Notification;
use atrium_api::types::TryFromUnknown;
use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_input::buttons;
use bsky_render::{theme, Color, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{WorkRequest, WorkResponse};

use crate::cdn::avatar_thumbnail_jpeg;
use crate::profile::ProfileScreen;
use crate::screen::{Screen, ScreenAction};
use crate::tabbar::{TabBar, TopLevel, TAB_BAR_HEIGHT};
use crate::thread::ThreadScreen;
use crate::widget::{Rect, UiCtx};

const HEADER_H: i32 = 40;
const VIEWPORT_TOP: i32 = HEADER_H;
const VIEWPORT_BOTTOM: i32 = SCREEN_HEIGHT - TAB_BAR_HEIGHT;
const VIEWPORT_H: i32 = VIEWPORT_BOTTOM - VIEWPORT_TOP;

const ROW_PAD_X: i32 = 16;
const ROW_PAD_Y: i32 = 14;
const NOTIF_AVATAR_SIZE: i32 = 40;
const ROW_TEXT_LEFT: i32 = ROW_PAD_X + NOTIF_AVATAR_SIZE + 12;
const PAGINATION_THRESHOLD: i32 = 600;

enum NotifState {
    Loading,
    Loaded {
        notifications: Vec<Notification>,
        next_cursor: Option<String>,
    },
    Error(String),
}

pub struct NotificationsScreen {
    client: Arc<AuthClient>,
    state: NotifState,
    scroll_y: f32,
    selected_idx: usize,
    dispatched: bool,
    fetching_more: bool,
    marked_seen: bool,
    row_heights: Vec<i32>,
    inflight_avatars: HashSet<String>,
    tab_bar: TabBar,
}

impl NotificationsScreen {
    pub fn new(client: Arc<AuthClient>) -> Self {
        Self {
            client,
            state: NotifState::Loading,
            scroll_y: 0.0,
            selected_idx: 0,
            dispatched: false,
            fetching_more: false,
            marked_seen: false,
            row_heights: Vec::new(),
            inflight_avatars: HashSet::new(),
            tab_bar: TabBar::new(TopLevel::Notifications),
        }
    }
}

impl Screen for NotificationsScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        _ime: &mut Ime,
    ) -> ScreenAction {
        // First-frame: dispatch fetch + mark seen.
        if !self.dispatched {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::FetchNotifications { cursor: None });
                self.dispatched = true;
            }
        }
        if !self.marked_seen {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::MarkSeen {
                    seen_at: atrium_api::types::string::Datetime::now(),
                });
                self.marked_seen = true;
            }
        }

        // Selection nav.
        let count = match &self.state {
            NotifState::Loaded { notifications, .. } => notifications.len(),
            _ => 0,
        };
        let mut selection_changed = false;
        if count > 0
            && (ctx.pad.just_pressed(buttons::UP) || ctx.pad.just_pressed(buttons::DOWN))
        {
            if ctx.pad.just_pressed(buttons::UP) && self.selected_idx > 0 {
                self.selected_idx -= 1;
                selection_changed = true;
            }
            if ctx.pad.just_pressed(buttons::DOWN) && self.selected_idx + 1 < count {
                self.selected_idx += 1;
                selection_changed = true;
            }
        }

        // Lazy row-height measure (uniform-height rows for now: each
        // notif row is fixed at 2 lines + padding ≈ NOTIF_AVATAR_SIZE +
        // ROW_PAD_Y * 2).
        if let NotifState::Loaded { notifications, .. } = &self.state {
            while self.row_heights.len() < notifications.len() {
                self.row_heights.push(NOTIF_AVATAR_SIZE + ROW_PAD_Y * 2);
            }
        }

        // Layout.
        let total_h: i32 = self.row_heights.iter().sum();
        let max_scroll = (total_h - VIEWPORT_H).max(0) as f32;
        if selection_changed && self.selected_idx < self.row_heights.len() {
            let row_top: i32 = self.row_heights[..self.selected_idx].iter().sum();
            let row_h = self.row_heights[self.selected_idx];
            let view_top = self.scroll_y as i32;
            let view_bottom = view_top + VIEWPORT_H;
            const SCROLL_MARGIN: i32 = 50;
            if row_top < view_top + SCROLL_MARGIN {
                self.scroll_y = (row_top - SCROLL_MARGIN).max(0) as f32;
            } else if row_top + row_h > view_bottom - SCROLL_MARGIN {
                self.scroll_y =
                    (row_top + row_h + SCROLL_MARGIN - VIEWPORT_H).max(0) as f32;
            }
        }
        let stick_y = ctx.pad.left_stick.1;
        let mag = stick_y.unsigned_abs() as f32;
        const STICK_DEADZONE: f32 = 32.0;
        if mag > STICK_DEADZONE {
            let sign: f32 = if stick_y < 0 { -1.0 } else { 1.0 };
            let effective = (mag - STICK_DEADZONE) * sign;
            self.scroll_y += effective / 24.0;
        }
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);

        // Pagination trigger.
        if !self.fetching_more {
            if let NotifState::Loaded {
                next_cursor: Some(cursor),
                ..
            } = &self.state
            {
                let near_bottom =
                    self.scroll_y as i32 + VIEWPORT_H + PAGINATION_THRESHOLD >= total_h;
                if near_bottom {
                    if let Some(worker) = ctx.worker {
                        worker.send(WorkRequest::FetchNotifications {
                            cursor: Some(cursor.clone()),
                        });
                        self.fetching_more = true;
                    }
                }
            }
        }

        // Avatar dispatch for visible rows.
        if let NotifState::Loaded { notifications, .. } = &self.state {
            if let Some(worker) = ctx.worker {
                let mut y_probe = HEADER_H - self.scroll_y as i32;
                for (notif, &row_h) in notifications.iter().zip(self.row_heights.iter())
                {
                    let row_bottom = y_probe + row_h;
                    if row_bottom > VIEWPORT_TOP && y_probe < VIEWPORT_BOTTOM {
                        if let Some(url) = notif.author.avatar.as_ref() {
                            let url = avatar_thumbnail_jpeg(url);
                            if !ctx.texture_cache.contains(&url)
                                && !self.inflight_avatars.contains(&url)
                            {
                                worker.send(WorkRequest::FetchImage { url: url.clone() });
                                self.inflight_avatars.insert(url);
                            }
                        }
                    }
                    y_probe += row_h;
                }
            }
        }

        // Render.
        match &self.state {
            NotifState::Loading => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2,
                    theme::TEXT_MUTED,
                    1.1,
                    "Loading notifications…",
                );
            }
            NotifState::Loaded { notifications, .. } => {
                if notifications.is_empty() {
                    frame.draw_text_centered(
                        font,
                        SCREEN_HEIGHT / 2,
                        theme::TEXT_MUTED,
                        1.1,
                        "No notifications yet",
                    );
                } else {
                    let mut y = HEADER_H - self.scroll_y as i32;
                    for (i, (notif, &row_h)) in
                        notifications.iter().zip(self.row_heights.iter()).enumerate()
                    {
                        let row_bottom = y + row_h;
                        if row_bottom > VIEWPORT_TOP && y < VIEWPORT_BOTTOM {
                            draw_notif_row(
                                frame,
                                font,
                                notif,
                                y,
                                row_h,
                                ctx,
                                i == self.selected_idx,
                            );
                        }
                        y += row_h;
                    }
                }
            }
            NotifState::Error(msg) => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 - 20,
                    theme::ERROR,
                    1.0,
                    "Could not load notifications",
                );
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2 + 20,
                    theme::TEXT_MUTED,
                    0.85,
                    msg,
                );
            }
        }

        // Sticky header.
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, HEADER_H as f32, theme::FIELD_BG);
        frame.draw_text_centered(font, 26, theme::TEXT_PRIMARY, 1.1, "Notifications");
        frame.fill_rect(
            0.0,
            HEADER_H as f32 - 1.0,
            SCREEN_WIDTH as f32,
            1.0,
            theme::TEXT_MUTED,
        );

        // Tab bar.
        if let Some(target) = self.tab_bar.render(frame, font, ctx) {
            return ScreenAction::SwitchTab(target);
        }

        // Tap detection. Two zones per row:
        //  - Avatar (left ~ROW_TEXT_LEFT pixels): always opens the
        //    author's profile, regardless of notification reason.
        //  - Rest of the row: reason-aware context (thread for
        //    engagement / mention / reply / quote, profile for follow).
        if !ctx.touches.is_empty() {
            // Exclude the bottom tab-bar band so content taps don't fall
            // through the bar (which is drawn on top and handles them).
            let touches: Vec<_> = ctx
                .touches
                .iter()
                .filter(|t| t.y < VIEWPORT_BOTTOM)
                .map(|t| (t.x, t.y))
                .collect();
            let mut tap_target: Option<NotifTapAction> = None;
            if let NotifState::Loaded { notifications, .. } = &self.state {
                let mut y_probe = HEADER_H - self.scroll_y as i32;
                for (notif, &row_h) in notifications.iter().zip(self.row_heights.iter())
                {
                    let row_bottom = y_probe + row_h;
                    if row_bottom > VIEWPORT_TOP && y_probe < VIEWPORT_BOTTOM {
                        let avatar_rect = Rect::new(
                            0.0,
                            y_probe as f32,
                            ROW_TEXT_LEFT as f32,
                            row_h as f32,
                        );
                        let rest_rect = Rect::new(
                            ROW_TEXT_LEFT as f32,
                            y_probe as f32,
                            (SCREEN_WIDTH - ROW_TEXT_LEFT) as f32,
                            row_h as f32,
                        );
                        if touches.iter().any(|&(x, y)| avatar_rect.contains(x, y)) {
                            tap_target = Some(NotifTapAction::OpenProfile(
                                notif.author.handle.as_str().to_string(),
                            ));
                            break;
                        }
                        if touches.iter().any(|&(x, y)| rest_rect.contains(x, y)) {
                            tap_target = Some(notif_tap_target(notif));
                            break;
                        }
                    }
                    y_probe += row_h;
                }
            }
            if let Some(action) = tap_target {
                return match action {
                    NotifTapAction::OpenThread(uri) => ScreenAction::Push(Box::new(
                        ThreadScreen::new(Arc::clone(&self.client), uri),
                    )),
                    NotifTapAction::OpenProfile(handle) => ScreenAction::Push(Box::new(
                        ProfileScreen::new(Arc::clone(&self.client), Some(handle)),
                    )),
                };
            }
        }

        ScreenAction::None
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        match resp {
            WorkResponse::Notifications(Ok(batch)) => {
                self.fetching_more = false;
                // MarkSeen response carries an empty batch; ignore so we
                // don't replace real notifications.
                if batch.notifications.is_empty() && batch.cursor.is_none() {
                    return;
                }
                match &mut self.state {
                    NotifState::Loaded {
                        notifications,
                        next_cursor,
                    } => {
                        notifications.extend(batch.notifications);
                        *next_cursor = batch.cursor;
                    }
                    _ => {
                        self.state = NotifState::Loaded {
                            notifications: batch.notifications,
                            next_cursor: batch.cursor,
                        };
                    }
                }
            }
            WorkResponse::Notifications(Err(e)) => {
                self.fetching_more = false;
                if matches!(self.state, NotifState::Loading) {
                    self.state = NotifState::Error(e);
                }
            }
            WorkResponse::Image { url, .. } => {
                self.inflight_avatars.remove(&url);
            }
            _ => {}
        }
    }

    fn top_level(&self) -> Option<TopLevel> {
        Some(TopLevel::Notifications)
    }
}

enum NotifTapAction {
    OpenThread(String),
    OpenProfile(String),
}

/// Decide what tapping a notification should do based on its reason.
fn notif_tap_target(notif: &Notification) -> NotifTapAction {
    let reason = notif.reason.as_str();
    match reason {
        // Engagement on a post → open that post's thread.
        "like" | "repost" | "quote" => match notif.reason_subject.as_deref() {
            Some(uri) => NotifTapAction::OpenThread(uri.to_string()),
            None => NotifTapAction::OpenProfile(notif.author.handle.as_str().to_string()),
        },
        // Mention or reply → the notification's URI is the post that
        // contains the mention / is the reply.
        "mention" | "reply" => NotifTapAction::OpenThread(notif.uri.clone()),
        // Follow → profile.
        "follow" => NotifTapAction::OpenProfile(notif.author.handle.as_str().to_string()),
        // Unknown reasons → fall back to the author's profile.
        _ => NotifTapAction::OpenProfile(notif.author.handle.as_str().to_string()),
    }
}

fn draw_notif_row(
    frame: &mut Frame,
    font: &Font,
    notif: &Notification,
    y_top: i32,
    row_h: i32,
    ctx: &UiCtx,
    is_selected: bool,
) {
    // Background tint for unread + selection.
    if !notif.is_read {
        // Faint accent tint for unread.
        frame.fill_rect(
            0.0,
            y_top as f32,
            SCREEN_WIDTH as f32,
            row_h as f32,
            unread_tint(),
        );
    }
    if is_selected {
        frame.fill_rect(
            0.0,
            y_top as f32,
            3.0,
            row_h as f32,
            theme::ACCENT,
        );
    }

    // Avatar (40×40).
    let avatar_x = ROW_PAD_X;
    let avatar_y = y_top + ROW_PAD_Y;
    let mask = if is_selected || !notif.is_read {
        ctx.avatar_mask_field
    } else {
        ctx.avatar_mask
    };
    let url_thumb = notif
        .author
        .avatar
        .as_deref()
        .map(avatar_thumbnail_jpeg);
    let painted_real = match url_thumb.as_deref() {
        Some(url) => match ctx.texture_cache.get(url) {
            Some(tex) => {
                let sx = NOTIF_AVATAR_SIZE as f32 / tex.width().max(1) as f32;
                let sy = NOTIF_AVATAR_SIZE as f32 / tex.height().max(1) as f32;
                frame.draw_texture_scale(tex, avatar_x as f32, avatar_y as f32, sx, sy);
                true
            }
            None => false,
        },
        None => false,
    };
    if !painted_real {
        frame.fill_rect(
            avatar_x as f32,
            avatar_y as f32,
            NOTIF_AVATAR_SIZE as f32,
            NOTIF_AVATAR_SIZE as f32,
            placeholder_color(notif.author.handle.as_str()),
        );
    }
    if let Some(mask_tex) = mask {
        let sx = NOTIF_AVATAR_SIZE as f32 / mask_tex.width().max(1) as f32;
        let sy = NOTIF_AVATAR_SIZE as f32 / mask_tex.height().max(1) as f32;
        frame.draw_texture_scale(
            mask_tex,
            avatar_x as f32,
            avatar_y as f32,
            sx,
            sy,
        );
    }

    // Headline + optional snippet.
    let display = notif
        .author
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| notif.author.handle.as_str());
    let headline = match notif.reason.as_str() {
        "like" => format!("{display} liked your post"),
        "repost" => format!("{display} reposted your post"),
        "follow" => format!("{display} followed you"),
        "mention" => format!("{display} mentioned you"),
        "reply" => format!("{display} replied to your post"),
        "quote" => format!("{display} quoted your post"),
        other => format!("{display} ({other})"),
    };
    let text_y = y_top + ROW_PAD_Y + 16;
    frame.draw_text(
        font,
        ROW_TEXT_LEFT,
        text_y,
        theme::TEXT_PRIMARY,
        1.0,
        &headline,
    );

    // Snippet for mentions / replies / quotes: the post text.
    let snippet = match notif.reason.as_str() {
        "mention" | "reply" | "quote" => extract_record_text(&notif.record),
        _ => None,
    };
    if let Some(text) = snippet {
        let head: String = text.chars().take(80).collect();
        let snippet_y = text_y + 22;
        frame.draw_text(
            font,
            ROW_TEXT_LEFT,
            snippet_y,
            theme::TEXT_MUTED,
            0.85,
            &head,
        );
    }

    // Separator at row bottom.
    frame.fill_rect(
        0.0,
        (y_top + row_h - 1) as f32,
        SCREEN_WIDTH as f32,
        1.0,
        theme::FIELD_BG,
    );
}

/// Try to extract `text` from a notification's record (which is a post
/// for reply / mention / quote reasons).
fn extract_record_text(record: &atrium_api::types::Unknown) -> Option<String> {
    use atrium_api::app::bsky::feed::post::RecordData;
    RecordData::try_from_unknown(record.clone())
        .ok()
        .map(|r| r.text)
}

/// Subtle ACCENT-tinted background for unread notifications.
fn unread_tint() -> Color {
    // Slightly more saturated than FIELD_BG, hinting at the accent.
    Color::rgb(0x18, 0x28, 0x4A)
}

/// Mirrors `timeline::placeholder_color`.
fn placeholder_color(handle: &str) -> Color {
    const PALETTE: [Color; 8] = [
        Color::rgb(0xF8, 0x9A, 0x9A),
        Color::rgb(0xF8, 0xC1, 0x9A),
        Color::rgb(0xF8, 0xE8, 0x9A),
        Color::rgb(0x9A, 0xF8, 0xA0),
        Color::rgb(0x9A, 0xE0, 0xF8),
        Color::rgb(0x9A, 0xA0, 0xF8),
        Color::rgb(0xC4, 0x9A, 0xF8),
        Color::rgb(0xF8, 0x9A, 0xE0),
    ];
    let mut h: u32 = 2166136261;
    for b in handle.bytes() {
        h = h.wrapping_mul(16777619) ^ b as u32;
    }
    PALETTE[(h as usize) % PALETTE.len()]
}

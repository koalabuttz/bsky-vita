//! Conversation list (DM inbox) — top-level.
//!
//! Reachable via the tab bar's DMs tab. Lists the user's conversations
//! (most-recent-activity first) with the other member's avatar, name,
//! and a snippet of the last message; unread convos get an ACCENT tint.
//! Tap the avatar to open that member's profile, or the row body to open
//! the conversation.
//!
//! Modeled on [`NotificationsScreen`](crate::notifications): same
//! scroll / selection / pagination / avatar-dispatch machinery. DMs are
//! served by the chat service proxy (see `bsky-worker`); a missing DM
//! app-password scope surfaces as a `DM_SCOPE:` error here.

use std::collections::HashSet;
use std::sync::Arc;

use atrium_api::chat::bsky::convo::defs::{ConvoView, ConvoViewLastMessageRefs};
use atrium_api::types::Union;
use bsky_auth::AuthClient;
use bsky_ime::Ime;
use bsky_input::buttons;
use bsky_render::{theme, Color, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{WorkRequest, WorkResponse};

use crate::cdn::avatar_thumbnail_jpeg;
use crate::conversation::ConversationScreen;
use crate::profile::ProfileScreen;
use crate::screen::{Screen, ScreenAction};
use crate::tabbar::{TabBar, TopLevel, TAB_BAR_HEIGHT};
use crate::widget::{Rect, UiCtx};

const HEADER_H: i32 = 40;
const VIEWPORT_TOP: i32 = HEADER_H;
const VIEWPORT_BOTTOM: i32 = SCREEN_HEIGHT - TAB_BAR_HEIGHT;
const VIEWPORT_H: i32 = VIEWPORT_BOTTOM - VIEWPORT_TOP;

const ROW_PAD_X: i32 = 16;
const ROW_PAD_Y: i32 = 14;
const CONVO_AVATAR_SIZE: i32 = 40;
const ROW_TEXT_LEFT: i32 = ROW_PAD_X + CONVO_AVATAR_SIZE + 12;
const PAGINATION_THRESHOLD: i32 = 600;

enum ListState {
    Loading,
    Loaded {
        convos: Vec<ConvoView>,
        next_cursor: Option<String>,
    },
    Error { msg: String, auth_scope: bool },
}

pub struct ConversationListScreen {
    client: Arc<AuthClient>,
    own_did: String,
    state: ListState,
    scroll_y: f32,
    selected_idx: usize,
    dispatched: bool,
    fetching_more: bool,
    row_heights: Vec<i32>,
    inflight_avatars: HashSet<String>,
    tab_bar: TabBar,
}

impl ConversationListScreen {
    pub fn new(client: Arc<AuthClient>) -> Self {
        let own_did = client.resolved.did.clone();
        Self {
            client,
            own_did,
            state: ListState::Loading,
            scroll_y: 0.0,
            selected_idx: 0,
            dispatched: false,
            fetching_more: false,
            row_heights: Vec::new(),
            inflight_avatars: HashSet::new(),
            tab_bar: TabBar::new(TopLevel::Messages),
        }
    }
}

impl Screen for ConversationListScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        _ime: &mut Ime,
    ) -> ScreenAction {
        // First-frame: fetch the convo list.
        if !self.dispatched {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::FetchConvos { cursor: None });
                self.dispatched = true;
            }
        }

        // Selection nav.
        let count = match &self.state {
            ListState::Loaded { convos, .. } => convos.len(),
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

        // Uniform-height rows (avatar + 2 text lines + padding).
        if let ListState::Loaded { convos, .. } = &self.state {
            while self.row_heights.len() < convos.len() {
                self.row_heights.push(CONVO_AVATAR_SIZE + ROW_PAD_Y * 2);
            }
        }

        // Layout + scroll.
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

        // Pagination.
        if !self.fetching_more {
            if let ListState::Loaded {
                next_cursor: Some(cursor),
                ..
            } = &self.state
            {
                let near_bottom =
                    self.scroll_y as i32 + VIEWPORT_H + PAGINATION_THRESHOLD >= total_h;
                if near_bottom {
                    if let Some(worker) = ctx.worker {
                        worker.send(WorkRequest::FetchConvos {
                            cursor: Some(cursor.clone()),
                        });
                        self.fetching_more = true;
                    }
                }
            }
        }

        // Avatar dispatch for visible rows.
        if let ListState::Loaded { convos, .. } = &self.state {
            if let Some(worker) = ctx.worker {
                let mut y_probe = HEADER_H - self.scroll_y as i32;
                for (convo, &row_h) in convos.iter().zip(self.row_heights.iter()) {
                    let row_bottom = y_probe + row_h;
                    if row_bottom > VIEWPORT_TOP && y_probe < VIEWPORT_BOTTOM {
                        if let Some(member) = primary_other(convo, &self.own_did) {
                            if let Some(url) = member.avatar.as_ref() {
                                let url = avatar_thumbnail_jpeg(url);
                                if !ctx.texture_cache.contains(&url)
                                    && !self.inflight_avatars.contains(&url)
                                {
                                    worker.send(WorkRequest::FetchImage { url: url.clone() });
                                    self.inflight_avatars.insert(url);
                                }
                            }
                        }
                    }
                    y_probe += row_h;
                }
            }
        }

        // Render.
        match &self.state {
            ListState::Loading => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2,
                    theme::TEXT_MUTED,
                    1.1,
                    "Loading conversations…",
                );
            }
            ListState::Loaded { convos, .. } => {
                if convos.is_empty() {
                    frame.draw_text_centered(
                        font,
                        SCREEN_HEIGHT / 2,
                        theme::TEXT_MUTED,
                        1.1,
                        "No conversations yet",
                    );
                } else {
                    let mut y = HEADER_H - self.scroll_y as i32;
                    for (i, (convo, &row_h)) in
                        convos.iter().zip(self.row_heights.iter()).enumerate()
                    {
                        let row_bottom = y + row_h;
                        if row_bottom > VIEWPORT_TOP && y < VIEWPORT_BOTTOM {
                            draw_convo_row(
                                frame,
                                font,
                                convo,
                                &self.own_did,
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
            ListState::Error { msg, auth_scope } => {
                let (title, detail) = if *auth_scope {
                    (
                        "Direct messages unavailable",
                        "This app password can't access DMs. Create one with chat \
                         access enabled in Bluesky settings, then log in again.",
                    )
                } else {
                    ("Could not load conversations", msg.as_str())
                };
                frame.draw_text_centered(font, SCREEN_HEIGHT / 2 - 30, theme::ERROR, 1.0, title);
                draw_wrapped_centered(frame, font, SCREEN_HEIGHT / 2 + 4, detail);
            }
        }

        // Sticky header.
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, HEADER_H as f32, theme::FIELD_BG);
        frame.draw_text_centered(font, 26, theme::TEXT_PRIMARY, 1.1, "Messages");
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

        // Tap detection: avatar → profile, row body → conversation.
        if !ctx.touches.is_empty() {
            let touches: Vec<_> = ctx
                .touches
                .iter()
                .filter(|t| t.y < VIEWPORT_BOTTOM)
                .map(|t| (t.x, t.y))
                .collect();
            let mut tap: Option<ListTap> = None;
            if let ListState::Loaded { convos, .. } = &self.state {
                let mut y_probe = HEADER_H - self.scroll_y as i32;
                for convo in convos.iter() {
                    let row_h = CONVO_AVATAR_SIZE + ROW_PAD_Y * 2;
                    let row_bottom = y_probe + row_h;
                    if row_bottom > VIEWPORT_TOP && y_probe < VIEWPORT_BOTTOM {
                        let avatar_rect =
                            Rect::new(0.0, y_probe as f32, ROW_TEXT_LEFT as f32, row_h as f32);
                        let rest_rect = Rect::new(
                            ROW_TEXT_LEFT as f32,
                            y_probe as f32,
                            (SCREEN_WIDTH - ROW_TEXT_LEFT) as f32,
                            row_h as f32,
                        );
                        if touches.iter().any(|&(x, y)| avatar_rect.contains(x, y)) {
                            if let Some(m) = primary_other(convo, &self.own_did) {
                                tap = Some(ListTap::Profile(m.handle.as_str().to_string()));
                            }
                            break;
                        }
                        if touches.iter().any(|&(x, y)| rest_rect.contains(x, y)) {
                            tap = Some(ListTap::Open(convo.clone()));
                            break;
                        }
                    }
                    y_probe += row_h;
                }
            }
            if let Some(tap) = tap {
                return match tap {
                    ListTap::Profile(handle) => ScreenAction::Push(Box::new(
                        ProfileScreen::new(Arc::clone(&self.client), Some(handle)),
                    )),
                    ListTap::Open(convo) => ScreenAction::Push(Box::new(
                        ConversationScreen::new(Arc::clone(&self.client), convo),
                    )),
                };
            }
        }

        ScreenAction::None
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        match resp {
            WorkResponse::Convos(Ok(batch)) => {
                self.fetching_more = false;
                match &mut self.state {
                    ListState::Loaded {
                        convos,
                        next_cursor,
                    } => {
                        convos.extend(batch.convos);
                        *next_cursor = batch.cursor;
                    }
                    _ => {
                        self.state = ListState::Loaded {
                            convos: batch.convos,
                            next_cursor: batch.cursor,
                        };
                    }
                }
            }
            WorkResponse::Convos(Err(e)) => {
                self.fetching_more = false;
                if matches!(self.state, ListState::Loading) {
                    let auth_scope = e.starts_with("DM_SCOPE:");
                    self.state = ListState::Error { msg: e, auth_scope };
                }
            }
            WorkResponse::Image { url, .. } => {
                self.inflight_avatars.remove(&url);
            }
            _ => {}
        }
    }

    fn top_level(&self) -> Option<TopLevel> {
        Some(TopLevel::Messages)
    }
}

enum ListTap {
    Profile(String),
    Open(ConvoView),
}

/// The first conversation member that isn't the logged-in user. For a
/// 1:1 convo this is the other person; for a group it's the first other
/// member (used for the row avatar). `None` only for a degenerate
/// self-only convo.
fn primary_other<'a>(
    convo: &'a ConvoView,
    own_did: &str,
) -> Option<&'a atrium_api::chat::bsky::actor::defs::ProfileViewBasic> {
    convo
        .members
        .iter()
        .find(|m| m.did.as_str() != own_did)
        .or_else(|| convo.members.first())
}

/// Display title for a convo row: other member's name, plus "+N" for
/// group conversations.
fn convo_title(convo: &ConvoView, own_did: &str) -> String {
    let others: Vec<&str> = convo
        .members
        .iter()
        .filter(|m| m.did.as_str() != own_did)
        .map(|m| {
            m.display_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| m.handle.as_str())
        })
        .collect();
    match others.len() {
        0 => "(you)".to_string(),
        1 => others[0].to_string(),
        n => format!("{} +{}", others[0], n - 1),
    }
}

/// Snippet of the conversation's last message for the row's second line.
fn last_message_snippet(convo: &ConvoView) -> String {
    match convo.last_message.as_ref() {
        Some(Union::Refs(ConvoViewLastMessageRefs::MessageView(m))) => {
            m.text.chars().take(80).collect()
        }
        Some(Union::Refs(ConvoViewLastMessageRefs::DeletedMessageView(_))) => {
            "(message deleted)".to_string()
        }
        Some(Union::Unknown(_)) | None => String::new(),
    }
}

fn draw_convo_row(
    frame: &mut Frame,
    font: &Font,
    convo: &ConvoView,
    own_did: &str,
    y_top: i32,
    row_h: i32,
    ctx: &UiCtx,
    is_selected: bool,
) {
    let unread = convo.unread_count > 0;
    if unread {
        frame.fill_rect(
            0.0,
            y_top as f32,
            SCREEN_WIDTH as f32,
            row_h as f32,
            unread_tint(),
        );
    }
    if is_selected {
        frame.fill_rect(0.0, y_top as f32, 3.0, row_h as f32, theme::ACCENT);
    }

    // Avatar (40×40).
    let avatar_x = ROW_PAD_X;
    let avatar_y = y_top + ROW_PAD_Y;
    let mask = if is_selected || unread {
        ctx.avatar_mask_field
    } else {
        ctx.avatar_mask
    };
    let member = primary_other(convo, own_did);
    let handle = member.map(|m| m.handle.as_str()).unwrap_or("");
    let url_thumb = member
        .and_then(|m| m.avatar.as_deref())
        .map(avatar_thumbnail_jpeg);
    let painted_real = match url_thumb.as_deref() {
        Some(url) => match ctx.texture_cache.get(url) {
            Some(tex) => {
                let sx = CONVO_AVATAR_SIZE as f32 / tex.width().max(1) as f32;
                let sy = CONVO_AVATAR_SIZE as f32 / tex.height().max(1) as f32;
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
            CONVO_AVATAR_SIZE as f32,
            CONVO_AVATAR_SIZE as f32,
            placeholder_color(handle),
        );
    }
    if let Some(mask_tex) = mask {
        let sx = CONVO_AVATAR_SIZE as f32 / mask_tex.width().max(1) as f32;
        let sy = CONVO_AVATAR_SIZE as f32 / mask_tex.height().max(1) as f32;
        frame.draw_texture_scale(mask_tex, avatar_x as f32, avatar_y as f32, sx, sy);
    }

    // Title + snippet.
    let title = convo_title(convo, own_did);
    let text_y = y_top + ROW_PAD_Y + 16;
    frame.draw_text(font, ROW_TEXT_LEFT, text_y, theme::TEXT_PRIMARY, 1.0, &title);
    let snippet = last_message_snippet(convo);
    if !snippet.is_empty() {
        let color = if unread {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_MUTED
        };
        frame.draw_text(font, ROW_TEXT_LEFT, text_y + 22, color, 0.85, &snippet);
    }

    // Separator.
    frame.fill_rect(
        0.0,
        (y_top + row_h - 1) as f32,
        SCREEN_WIDTH as f32,
        1.0,
        theme::FIELD_BG,
    );
}

/// Center-draw a wrapped error/detail line under a title.
fn draw_wrapped_centered(frame: &mut Frame, font: &Font, y: i32, text: &str) {
    let max_w = SCREEN_WIDTH - 80;
    frame.draw_text_wrapped(
        font,
        40,
        y,
        max_w,
        theme::TEXT_MUTED,
        0.85,
        text,
    );
}

/// Subtle ACCENT-tinted background for unread rows (mirrors notifications).
fn unread_tint() -> Color {
    Color::rgb(0x18, 0x28, 0x4A)
}

/// Stable pastel color per handle (mirrors `timeline::placeholder_color`).
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

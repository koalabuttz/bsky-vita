//! Single conversation view — pushed sub-screen.
//!
//! Opened from [`ConversationListScreen`](crate::conversations) or the
//! profile "Message" button. Shows a scrollable list of message bubbles
//! (own messages right-aligned in ACCENT, others left in FIELD_BG) with
//! an inline compose bar at the bottom. CIRCLE pops back.
//!
//! Runtime behavior:
//! - Messages are kept oldest→newest (the worker normalizes API order).
//! - The newest page auto-refreshes every ~4s while the screen is open
//!   (paused while the IME modal is up), so incoming messages appear
//!   without input. We only auto-scroll to the bottom if the user is
//!   already there — otherwise we leave them reading history.
//! - Scrolling to the top paginates older history, preserving the
//!   on-screen position.
//! - Sending is optimistic: the bubble appears immediately and is
//!   reconciled when the server echoes it back (via the send response or
//!   the next poll, deduped by message id).
//!
//! Only one message-fetch (initial / poll / older) is ever in flight at
//! once, which is how a `ConvoMessages` response is attributed to the
//! request that produced it (the response echoes only `convo_id`).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_api::chat::bsky::actor::defs::ProfileViewBasic;
use atrium_api::chat::bsky::convo::defs::ConvoView;
use bsky_auth::AuthClient;
use bsky_ime::{Ime, ImeMode, ImeState, TextBoxMode};
use bsky_input::buttons;
use bsky_render::{theme, Color, Font, Frame, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::{MessageItem, WorkRequest, WorkResponse};

use crate::profile::ProfileScreen;
use crate::screen::{Screen, ScreenAction};
use crate::tabbar::TopLevel;
use crate::widget::{button, ButtonState, Rect, UiCtx};

const HEADER_H: i32 = 40;
const COMPOSE_H: i32 = 56;
const VIEWPORT_TOP: i32 = HEADER_H;
const VIEWPORT_BOTTOM: i32 = SCREEN_HEIGHT - COMPOSE_H;
const VIEWPORT_H: i32 = VIEWPORT_BOTTOM - VIEWPORT_TOP;

const BUBBLE_MARGIN: i32 = 12;
const BUBBLE_MAX_W: i32 = 600;
const BUBBLE_PAD_X: i32 = 12;
const BUBBLE_PAD_Y: i32 = 8;
const ROW_GAP: i32 = 10;
const NAME_LINE_H: i32 = 18;
const MSG_SCALE: f32 = 1.0;

/// Graphemes-ish cap for a DM (the lexicon allows 1000).
const MSG_LIMIT: usize = 1000;
/// How close to the bottom counts as "at the bottom" for auto-scroll.
const BOTTOM_STICKY_MARGIN: i32 = 120;
/// How close to the top triggers older-history pagination.
const TOP_PAGINATE_MARGIN: i32 = 80;
/// Auto-refresh cadence for the newest page.
const POLL_INTERVAL: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, PartialEq)]
enum Delivery {
    /// Confirmed on the server (from getMessages or a successful send).
    Sent,
    /// Optimistic local row, SendMessage in flight.
    Sending,
}

/// One display row. `id: None` marks an optimistic-pending local message.
struct Msg {
    id: Option<String>,
    local_key: u64,
    sender_did: String,
    is_own: bool,
    text: String,
    deleted: bool,
    delivery: Delivery,
}

enum Phase {
    InitialLoading,
    Ready,
    InitialError { msg: String, auth_scope: bool },
}

/// Captures where to restore the scroll after older messages are
/// prepended above the viewport.
struct PrependAnchor {
    anchor_key: u64,
    scroll_y_before: f32,
}

pub struct ConversationScreen {
    client: Arc<AuthClient>,
    convo_id: String,
    own_did: String,
    members: Vec<ProfileViewBasic>,
    title: String,
    is_group: bool,

    phase: Phase,
    messages: Vec<Msg>,
    seen_ids: HashSet<String>,
    /// Per-message measured heights; kept in lockstep with `messages`
    /// (`row_heights.len()` is the measured watermark). Cleared on
    /// prepend to force a remeasure.
    row_heights: Vec<i32>,

    top_cursor: Option<String>,
    reached_top: bool,

    scroll_y: f32,
    pending_scroll_to_bottom: bool,
    pending_prepend: Option<PrependAnchor>,

    initial_inflight: bool,
    poll_inflight: bool,
    older_inflight: bool,
    send_inflight: bool,
    last_poll: Instant,
    want_mark_read: bool,

    draft: String,
    compose_btn: ButtonState,
    send_btn: ButtonState,
    header_btn: ButtonState,
    next_local_key: u64,
    auth_blocked: bool,
}

impl ConversationScreen {
    pub fn new(client: Arc<AuthClient>, convo: ConvoView) -> Self {
        let own_did = client.resolved.did.clone();
        let members = convo.members.clone();
        let is_group = members.len() > 2;
        let title = build_title(&members, &own_did);
        Self {
            client,
            convo_id: convo.id.clone(),
            own_did,
            members,
            title,
            is_group,
            phase: Phase::InitialLoading,
            messages: Vec::new(),
            seen_ids: HashSet::new(),
            row_heights: Vec::new(),
            top_cursor: None,
            reached_top: false,
            scroll_y: 0.0,
            pending_scroll_to_bottom: false,
            pending_prepend: None,
            initial_inflight: false,
            poll_inflight: false,
            older_inflight: false,
            send_inflight: false,
            last_poll: Instant::now(),
            want_mark_read: false,
            draft: String::new(),
            compose_btn: ButtonState::default(),
            send_btn: ButtonState::default(),
            header_btn: ButtonState::default(),
            next_local_key: 1,
            auth_blocked: false,
        }
    }

    /// Handle of the first member that isn't the logged-in user (for the
    /// header-tap → profile navigation).
    fn primary_other_handle(&self) -> Option<String> {
        self.members
            .iter()
            .find(|m| m.did.as_str() != self.own_did)
            .map(|m| m.handle.as_str().to_string())
    }

    fn next_key(&mut self) -> u64 {
        let k = self.next_local_key;
        self.next_local_key += 1;
        k
    }

    fn msg_from_item(&mut self, item: &MessageItem) -> Msg {
        let key = self.next_key();
        match item {
            MessageItem::Message(m) => {
                let did = m.sender.did.as_str().to_string();
                Msg {
                    id: Some(m.id.clone()),
                    local_key: key,
                    is_own: did == self.own_did,
                    sender_did: did,
                    text: m.text.clone(),
                    deleted: false,
                    delivery: Delivery::Sent,
                }
            }
            MessageItem::Deleted(d) => {
                let did = d.sender.did.as_str().to_string();
                Msg {
                    id: Some(d.id.clone()),
                    local_key: key,
                    is_own: did == self.own_did,
                    sender_did: did,
                    text: String::new(),
                    deleted: true,
                    delivery: Delivery::Sent,
                }
            }
        }
    }

    /// Try to reconcile a server message id against an in-flight
    /// optimistic row with the same text. Returns true if reconciled
    /// (so the caller should NOT also append it).
    fn reconcile_optimistic(&mut self, id: &str, text: &str) -> bool {
        if let Some(m) = self.messages.iter_mut().find(|m| {
            m.is_own && m.id.is_none() && m.delivery == Delivery::Sending && m.text == text
        }) {
            m.id = Some(id.to_string());
            m.delivery = Delivery::Sent;
            self.seen_ids.insert(id.to_string());
            true
        } else {
            false
        }
    }
}

impl Screen for ConversationScreen {
    fn frame(
        &mut self,
        frame: &mut Frame,
        font: &Font,
        ctx: &UiCtx,
        ime: &mut Ime,
    ) -> ScreenAction {
        // ── 0. Initial dispatch ──
        if matches!(self.phase, Phase::InitialLoading) && !self.initial_inflight {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::GetConvoMessages {
                    convo_id: self.convo_id.clone(),
                    cursor: None,
                });
                self.initial_inflight = true;
            }
        }

        // ── 1. Drain IME → draft ──
        match ime.poll() {
            ImeState::Finished(s) => {
                self.draft = s;
                ime.close();
            }
            ImeState::Cancelled | ImeState::Aborted => {
                ime.close();
            }
            _ => {}
        }

        // ── 2. Input (only when the IME modal isn't up) ──
        if !ime.is_active() {
            if ctx.pad.just_pressed(buttons::CIRCLE) {
                return ScreenAction::Pop;
            }
            if ctx.pad.just_pressed(buttons::UP) {
                self.scroll_y -= 40.0;
            }
            if ctx.pad.just_pressed(buttons::DOWN) {
                self.scroll_y += 40.0;
            }
            let stick_y = ctx.pad.left_stick.1;
            let mag = stick_y.unsigned_abs() as f32;
            const STICK_DEADZONE: f32 = 32.0;
            if mag > STICK_DEADZONE {
                let sign: f32 = if stick_y < 0 { -1.0 } else { 1.0 };
                self.scroll_y += (mag - STICK_DEADZONE) * sign / 24.0;
            }
        }

        // ── 3. Poll timer ──
        if matches!(self.phase, Phase::Ready)
            && !self.initial_inflight
            && !self.older_inflight
            && !self.poll_inflight
            && !ime.is_active()
            && self.last_poll.elapsed() >= POLL_INTERVAL
        {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::GetConvoMessages {
                    convo_id: self.convo_id.clone(),
                    cursor: None,
                });
                self.poll_inflight = true;
            }
        }

        // ── 4. Older-history pagination ──
        if matches!(self.phase, Phase::Ready)
            && !self.initial_inflight
            && !self.older_inflight
            && !self.poll_inflight
            && !self.reached_top
            && self.top_cursor.is_some()
            && self.scroll_y <= TOP_PAGINATE_MARGIN as f32
        {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::GetConvoMessages {
                    convo_id: self.convo_id.clone(),
                    cursor: self.top_cursor.clone(),
                });
                self.older_inflight = true;
            }
        }

        // ── 5. Mark-read dispatch ──
        if self.want_mark_read {
            if let Some(worker) = ctx.worker {
                worker.send(WorkRequest::MarkConvoRead {
                    convo_id: self.convo_id.clone(),
                });
                self.want_mark_read = false;
            }
        }

        // ── 6. Lazy-measure rows from the watermark ──
        while self.row_heights.len() < self.messages.len() {
            let i = self.row_heights.len();
            let h = measure_msg(frame, font, &self.messages[i], ctx.emoji, self.is_group);
            self.row_heights.push(h);
        }

        // ── 7. Scroll math + one-shot anchors ──
        let total_h: i32 = self.row_heights.iter().sum();
        let max_scroll = (total_h - VIEWPORT_H).max(0) as f32;
        if self.pending_scroll_to_bottom {
            self.scroll_y = max_scroll;
            self.pending_scroll_to_bottom = false;
        }
        if let Some(anchor) = self.pending_prepend.take() {
            if let Some(idx) = self
                .messages
                .iter()
                .position(|m| m.local_key == anchor.anchor_key)
            {
                let prepend_h: i32 = self.row_heights[..idx.min(self.row_heights.len())]
                    .iter()
                    .sum();
                self.scroll_y = anchor.scroll_y_before + prepend_h as f32;
            }
        }
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);

        // ── 8. Render message bubbles (visible only) ──
        match &self.phase {
            Phase::InitialLoading => {
                frame.draw_text_centered(
                    font,
                    SCREEN_HEIGHT / 2,
                    theme::TEXT_MUTED,
                    1.1,
                    "Loading…",
                );
            }
            Phase::InitialError { msg, auth_scope } => {
                let (title, detail) = if *auth_scope {
                    (
                        "Direct messages unavailable",
                        "This app password can't access DMs. Create one with chat \
                         access enabled in Bluesky settings, then log in again.",
                    )
                } else {
                    ("Could not load messages", msg.as_str())
                };
                frame.draw_text_centered(font, SCREEN_HEIGHT / 2 - 30, theme::ERROR, 1.0, title);
                frame.draw_text_wrapped(
                    font,
                    40,
                    SCREEN_HEIGHT / 2 + 4,
                    SCREEN_WIDTH - 80,
                    theme::TEXT_MUTED,
                    0.85,
                    detail,
                );
            }
            Phase::Ready => {
                if self.messages.is_empty() {
                    frame.draw_text_centered(
                        font,
                        SCREEN_HEIGHT / 2,
                        theme::TEXT_MUTED,
                        1.05,
                        "No messages yet. Say hi!",
                    );
                } else {
                    let mut y = HEADER_H - self.scroll_y as i32;
                    for (msg, &row_h) in self.messages.iter().zip(self.row_heights.iter()) {
                        if y + row_h > VIEWPORT_TOP && y < VIEWPORT_BOTTOM {
                            draw_msg(
                                frame,
                                font,
                                msg,
                                y,
                                ctx,
                                self.is_group,
                                &self.members,
                            );
                        }
                        y += row_h;
                    }
                }
            }
        }

        // ── 9. Sticky header (over scrolled content) ──
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, HEADER_H as f32, theme::FIELD_BG);
        frame.draw_text_centered(font, 26, theme::TEXT_PRIMARY, 1.05, &self.title);
        frame.fill_rect(
            0.0,
            HEADER_H as f32 - 1.0,
            SCREEN_WIDTH as f32,
            1.0,
            theme::TEXT_MUTED,
        );

        // ── 10. Compose bar (only in Ready) ──
        if matches!(self.phase, Phase::Ready) {
            let interactive = !ime.is_active();
            let cy = (SCREEN_HEIGHT - COMPOSE_H) as f32;
            frame.fill_rect(0.0, cy, SCREEN_WIDTH as f32, COMPOSE_H as f32, theme::FIELD_BG);
            frame.fill_rect(0.0, cy, SCREEN_WIDTH as f32, 1.0, theme::TEXT_MUTED);

            // Send button (right).
            let send_w = 84;
            let send_x = SCREEN_WIDTH - send_w - 12;
            let btn_y = SCREEN_HEIGHT - COMPOSE_H + 8;
            let can_send =
                interactive && !self.draft.is_empty() && !self.send_inflight && !self.auth_blocked;
            let send_rect =
                Rect::new(send_x as f32, btn_y as f32, send_w as f32, 40.0);
            let send_clicked =
                button(frame, font, send_rect, "Send", &mut self.send_btn, ctx, can_send);

            // Text region (left of the button).
            let text_x = 12;
            let text_w = send_x - 12 - text_x;
            let text_rect = Rect::new(text_x as f32, btn_y as f32, text_w as f32, 40.0);
            frame.fill_rect(
                text_rect.x,
                text_rect.y,
                text_rect.w,
                text_rect.h,
                theme::FIELD_BG_FOCUS,
            );
            let (display, color) = if self.auth_blocked {
                (
                    "DMs need a chat-enabled app password".to_string(),
                    theme::ERROR,
                )
            } else if self.draft.is_empty() {
                ("Message…".to_string(), theme::TEXT_MUTED)
            } else {
                (self.draft.clone(), theme::TEXT_PRIMARY)
            };
            let shown = truncate_to_width(frame, font, &display, 1.0, text_w - 20);
            frame.draw_text(font, text_x + 10, btn_y + 26, color, 1.0, &shown);

            // Tap the text region → open IME with the current draft.
            let region_pressed = ctx.touches.iter().any(|t| text_rect.contains(t.x, t.y));
            let region_clicked = interactive
                && self.compose_btn.pressed_last
                && !region_pressed
                && ctx.touches.is_empty();
            self.compose_btn.pressed_last = region_pressed;
            if region_clicked && !self.auth_blocked {
                let _ = ime.open(
                    "Message",
                    ImeMode::Default,
                    TextBoxMode::Default,
                    MSG_LIMIT as u32,
                    &self.draft,
                );
            }

            // Send.
            if send_clicked && can_send {
                let text = std::mem::take(&mut self.draft);
                let key = self.next_key();
                self.messages.push(Msg {
                    id: None,
                    local_key: key,
                    sender_did: self.own_did.clone(),
                    is_own: true,
                    text: text.clone(),
                    deleted: false,
                    delivery: Delivery::Sending,
                });
                self.send_inflight = true;
                self.pending_scroll_to_bottom = true;
                if let Some(worker) = ctx.worker {
                    worker.send(WorkRequest::SendMessage {
                        convo_id: self.convo_id.clone(),
                        text,
                    });
                }
            }
        }

        // ── 11. Header tap → other member's profile ──
        let header_rect = Rect::new(0.0, 0.0, SCREEN_WIDTH as f32, HEADER_H as f32);
        let header_pressed = ctx.touches.iter().any(|t| header_rect.contains(t.x, t.y));
        let header_clicked = !ime.is_active()
            && self.header_btn.pressed_last
            && !header_pressed
            && ctx.touches.is_empty();
        self.header_btn.pressed_last = header_pressed;
        if header_clicked {
            if let Some(handle) = self.primary_other_handle() {
                return ScreenAction::Push(Box::new(ProfileScreen::new(
                    Arc::clone(&self.client),
                    Some(handle),
                )));
            }
        }

        ScreenAction::None
    }

    fn handle_worker_response(&mut self, resp: WorkResponse) {
        match resp {
            WorkResponse::ConvoMessages { convo_id, batch } => {
                if convo_id != self.convo_id {
                    return;
                }
                // Exactly one fetch is in flight (dispatch guards ensure
                // it), so the set flag tells us which request this was.
                enum Which {
                    Initial,
                    Older,
                    Poll,
                }
                let which = if self.initial_inflight {
                    self.initial_inflight = false;
                    Which::Initial
                } else if self.older_inflight {
                    self.older_inflight = false;
                    Which::Older
                } else if self.poll_inflight {
                    self.poll_inflight = false;
                    self.last_poll = Instant::now();
                    Which::Poll
                } else {
                    return; // stale / unattributable
                };

                match batch {
                    Ok(b) => match which {
                        Which::Initial => {
                            for item in &b.messages {
                                let m = self.msg_from_item(item);
                                if let Some(id) = &m.id {
                                    self.seen_ids.insert(id.clone());
                                }
                                self.messages.push(m);
                            }
                            self.top_cursor = b.cursor;
                            self.reached_top = self.top_cursor.is_none();
                            self.phase = Phase::Ready;
                            self.pending_scroll_to_bottom = true;
                            if !self.messages.is_empty() {
                                self.want_mark_read = true;
                            }
                        }
                        Which::Poll => {
                            let max_scroll_before =
                                (self.row_heights.iter().sum::<i32>() - VIEWPORT_H).max(0) as f32;
                            let was_at_bottom = self.scroll_y
                                >= max_scroll_before - BOTTOM_STICKY_MARGIN as f32;
                            let mut new_arrivals = 0;
                            for item in &b.messages {
                                let m = self.msg_from_item(item);
                                if let Some(id) = m.id.clone() {
                                    if self.seen_ids.contains(&id) {
                                        continue;
                                    }
                                    if m.is_own && self.reconcile_optimistic(&id, &m.text) {
                                        continue;
                                    }
                                    self.seen_ids.insert(id);
                                    self.messages.push(m);
                                    new_arrivals += 1;
                                }
                            }
                            if new_arrivals > 0 && was_at_bottom {
                                self.pending_scroll_to_bottom = true;
                                self.want_mark_read = true;
                            }
                        }
                        Which::Older => {
                            let anchor_key = self.messages.first().map(|m| m.local_key);
                            let scroll_y_before = self.scroll_y;
                            let mut older: Vec<Msg> = Vec::new();
                            for item in &b.messages {
                                let m = self.msg_from_item(item);
                                if let Some(id) = &m.id {
                                    if self.seen_ids.contains(id) {
                                        continue;
                                    }
                                    self.seen_ids.insert(id.clone());
                                }
                                older.push(m);
                            }
                            if !older.is_empty() {
                                let n = older.len();
                                self.messages.splice(0..0, older);
                                // Indices shifted — force a full remeasure.
                                self.row_heights.clear();
                                if let Some(anchor_key) = anchor_key {
                                    self.pending_prepend = Some(PrependAnchor {
                                        anchor_key,
                                        scroll_y_before,
                                    });
                                } else {
                                    // No prior anchor (was empty) — pin bottom.
                                    let _ = n;
                                    self.pending_scroll_to_bottom = true;
                                }
                            }
                            self.top_cursor = b.cursor;
                            self.reached_top = self.top_cursor.is_none();
                        }
                    },
                    Err(e) => {
                        let auth_scope = e.starts_with("DM_SCOPE:");
                        match which {
                            Which::Initial => {
                                self.phase = Phase::InitialError { msg: e, auth_scope };
                            }
                            _ => {
                                if auth_scope {
                                    self.auth_blocked = true;
                                }
                                // Transient poll/older failure: logged by the
                                // worker; leave the Ready screen intact.
                            }
                        }
                    }
                }
            }
            WorkResponse::MessageSent { convo_id, result } => {
                if convo_id != self.convo_id {
                    return;
                }
                self.send_inflight = false;
                match result {
                    Ok(view) => {
                        let id = view.id.clone();
                        let text = view.text.clone();
                        if !self.reconcile_optimistic(&id, &text) && !self.seen_ids.contains(&id) {
                            // Already reconciled by a poll, or genuinely new.
                            let key = self.next_key();
                            self.seen_ids.insert(id.clone());
                            self.messages.push(Msg {
                                id: Some(id),
                                local_key: key,
                                sender_did: view.sender.did.as_str().to_string(),
                                is_own: true,
                                text,
                                deleted: false,
                                delivery: Delivery::Sent,
                            });
                            self.pending_scroll_to_bottom = true;
                        }
                    }
                    Err(e) => {
                        if e.starts_with("DM_SCOPE:") {
                            self.auth_blocked = true;
                        }
                        // Drop the optimistic row and restore the text to the
                        // draft (if the user hasn't started a new one) so they
                        // can retry with Send.
                        if let Some(pos) = self.messages.iter().position(|m| {
                            m.is_own && m.id.is_none() && m.delivery == Delivery::Sending
                        }) {
                            let failed = self.messages.remove(pos);
                            self.row_heights.clear(); // indices shifted
                            if self.draft.is_empty() {
                                self.draft = failed.text;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn top_level(&self) -> Option<TopLevel> {
        None
    }
}

/// Conversation title: the other member's name (group: "name +N").
fn build_title(members: &[ProfileViewBasic], own_did: &str) -> String {
    let others: Vec<String> = members
        .iter()
        .filter(|m| m.did.as_str() != own_did)
        .map(|m| member_display(m))
        .collect();
    match others.len() {
        0 => "(you)".to_string(),
        1 => others[0].clone(),
        n => format!("{} +{}", others[0], n - 1),
    }
}

fn member_display(m: &ProfileViewBasic) -> String {
    m.display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| m.handle.as_str())
        .to_string()
}

fn member_name(members: &[ProfileViewBasic], did: &str) -> String {
    members
        .iter()
        .find(|m| m.did.as_str() == did)
        .map(member_display)
        .unwrap_or_else(|| short_did(did))
}

fn short_did(did: &str) -> String {
    let n = did.len();
    if n > 10 {
        format!("…{}", &did[n - 8..])
    } else {
        did.to_string()
    }
}

/// Measured height of a message row (bubble + padding + group name line
/// + bottom gap).
fn measure_msg(
    frame: &Frame,
    font: &Font,
    msg: &Msg,
    emoji: Option<&bsky_render::EmojiAtlas>,
    is_group: bool,
) -> i32 {
    let ref_h = frame.measure_text(font, MSG_SCALE, "Hg").1;
    let line_h = ref_h + 4;
    let text = bubble_text(msg);
    let text_w_max = BUBBLE_MAX_W - 2 * BUBBLE_PAD_X;
    let block = if text.is_empty() {
        line_h
    } else {
        frame.measure_text_wrapped_with_emoji(font, text_w_max, MSG_SCALE, &text, emoji)
    };
    let mut h = block + 2 * BUBBLE_PAD_Y;
    if is_group && !msg.is_own {
        h += NAME_LINE_H;
    }
    h + ROW_GAP
}

fn draw_msg(
    frame: &mut Frame,
    font: &Font,
    msg: &Msg,
    y_top: i32,
    ctx: &UiCtx,
    is_group: bool,
    members: &[ProfileViewBasic],
) {
    let ref_h = frame.measure_text(font, MSG_SCALE, "Hg").1;
    let text = bubble_text(msg);
    let text_w_max = BUBBLE_MAX_W - 2 * BUBBLE_PAD_X;
    let content_w = frame
        .measure_text_wrapped_width_with_emoji(font, text_w_max, MSG_SCALE, &text, ctx.emoji)
        .max(8);
    let block_h =
        frame.measure_text_wrapped_with_emoji(font, text_w_max, MSG_SCALE, &text, ctx.emoji);
    let bubble_w = content_w + 2 * BUBBLE_PAD_X;
    let bubble_h = block_h + 2 * BUBBLE_PAD_Y;

    let mut top = y_top;
    if is_group && !msg.is_own {
        let name = member_name(members, &msg.sender_did);
        frame.draw_text(font, BUBBLE_MARGIN, y_top + 14, theme::TEXT_MUTED, 0.8, &name);
        top += NAME_LINE_H;
    }

    let bubble_x = if msg.is_own {
        SCREEN_WIDTH - BUBBLE_MARGIN - bubble_w
    } else {
        BUBBLE_MARGIN
    };
    let bubble_color = if msg.deleted {
        theme::FIELD_BG
    } else if msg.is_own {
        match msg.delivery {
            // Dimmed accent while sending.
            Delivery::Sending => Color::rgb(0x0E, 0x6F, 0xCC),
            Delivery::Sent => theme::ACCENT,
        }
    } else {
        theme::FIELD_BG
    };
    frame.fill_rect(
        bubble_x as f32,
        top as f32,
        bubble_w as f32,
        bubble_h as f32,
        bubble_color,
    );
    let text_color = if msg.deleted {
        theme::TEXT_MUTED
    } else {
        theme::TEXT_PRIMARY
    };
    frame.draw_text_wrapped_with_emoji(
        font,
        bubble_x + BUBBLE_PAD_X,
        top + BUBBLE_PAD_Y + ref_h,
        text_w_max,
        text_color,
        MSG_SCALE,
        &text,
        ctx.emoji,
    );
}

fn bubble_text(msg: &Msg) -> String {
    if msg.deleted {
        "(message deleted)".to_string()
    } else {
        msg.text.clone()
    }
}

/// Greedy truncation with an ellipsis (mirrors widget::truncate_to_width).
fn truncate_to_width(frame: &Frame, font: &Font, text: &str, scale: f32, max_w: i32) -> String {
    let (full_w, _) = frame.measure_text(font, scale, text);
    if full_w <= max_w {
        return text.to_string();
    }
    let mut s = text.to_string();
    while !s.is_empty() {
        s.pop();
        let candidate = format!("{s}…");
        let (w, _) = frame.measure_text(font, scale, &candidate);
        if w <= max_w {
            return candidate;
        }
    }
    String::from("…")
}

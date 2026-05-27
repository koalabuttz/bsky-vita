//! Filesystem image picker — a thumbnail-grid browser.
//!
//! The Vita has no native photo-picker dialog (vitasdk only exposes
//! `scePhotoExport*`, which saves *to* the gallery), so this is a custom
//! browser over `bsky_media::fs::read_dir`. It's a reusable *component*,
//! not a top-level `Screen`: `ComposeScreen` owns one and shows it
//! modally, so the picked path is returned in-process (the nav stack's
//! `Pop` carries no value).
//!
//! Phase 7 step 2: navigation + grid layout with folder/file cells, names
//! only. Step 3 swaps the file cells' placeholder box for a decoded image
//! thumbnail.
//!
//! Navigation: D-pad moves the grid cursor; CROSS enters a folder or
//! picks a file; CIRCLE goes up a directory (or cancels at a mount root).
//! Shortcut chips jump to Photos / Downloads / Browse roots. Touch taps
//! work on chips and cells directly.

use std::collections::HashMap;

use bsky_input::buttons;
use bsky_media::fs::{self, DirEntry};
use bsky_render::{theme, Font, Frame, Texture, SCREEN_HEIGHT, SCREEN_WIDTH};
use bsky_worker::WorkRequest;

use crate::widget::{ButtonState, Rect, UiCtx};

/// Result of a finished picker interaction.
pub enum PickResult {
    /// A file was chosen; carries its full Sce path (e.g. `ux0:picture/a.jpg`).
    Picked(String),
    /// The user backed out without choosing.
    Cancelled,
}

// Shortcut roots. `ux0:picture/` is the system gallery; `ux0:download/`
// is where the browser drops files; `ux0:` is the memory-card root for
// free-form browsing ("anywhere on the machine").
const SHORTCUTS: [(&str, &str); 3] = [
    ("Photos", "ux0:picture/"),
    ("Downloads", "ux0:download/"),
    ("Browse", "ux0:"),
];

const COLS: usize = 4;
const MARGIN_X: i32 = 14;
const GAP: i32 = 10;
const CELL_W: i32 = 225;
const CELL_H: i32 = 128;
const GRID_TOP: i32 = 78;
const GRID_BOTTOM: i32 = SCREEN_HEIGHT - 26;

/// Thumbnail render area within a cell (above the filename label).
const THUMB_AREA_W: i32 = CELL_W - 12;
const THUMB_AREA_H: i32 = CELL_H - 6 - 26;

/// Per-cell thumbnail load state.
enum ThumbState {
    Loading,
    Ready(Texture),
    Failed,
}

/// Rows of the grid visible at once.
const VISIBLE_ROWS: usize = ((GRID_BOTTOM - GRID_TOP + GAP) / (CELL_H + GAP)) as usize;

pub struct FilePicker {
    /// Current directory (Sce path; root form `drive:` or `drive:sub/...`).
    path: String,
    /// Directory entries (folders + image files), folders first, sorted.
    entries: Vec<DirEntry>,
    /// Read error for the current path, if any.
    error: Option<String>,
    /// Grid cursor (index into `entries`).
    cursor: usize,
    /// Topmost visible grid row.
    scroll_row: usize,
    /// Lazy-load flag — entries are (re)read on the first frame after a
    /// path change.
    loaded: bool,
    /// Tap state for the three shortcut chips.
    chip_btns: [ButtonState; 3],
    /// Tap state per entry cell (sized to `entries` on load).
    cell_btns: Vec<ButtonState>,
    /// Decoded thumbnails keyed by full file path. Cleared on navigate so
    /// a dir's worth of small textures is the memory ceiling.
    thumbs: HashMap<String, ThumbState>,
}

impl FilePicker {
    /// Open the picker at the Photos shortcut.
    pub fn new() -> Self {
        Self {
            path: SHORTCUTS[0].1.to_string(),
            entries: Vec::new(),
            error: None,
            cursor: 0,
            scroll_row: 0,
            loaded: false,
            chip_btns: Default::default(),
            cell_btns: Vec::new(),
            thumbs: HashMap::new(),
        }
    }

    /// Switch to `path` and reload on the next frame.
    fn navigate(&mut self, path: String) {
        self.path = path;
        self.loaded = false;
    }

    /// (Re)read the current directory: keep folders + image files, folders
    /// first then case-insensitive by name.
    fn load(&mut self) {
        match fs::read_dir(&self.path) {
            Ok(mut es) => {
                es.retain(|e| e.is_dir || is_image(&e.name));
                es.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                });
                self.entries = es;
                self.error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.error = Some(e.to_string());
            }
        }
        self.cursor = 0;
        self.scroll_row = 0;
        self.cell_btns = (0..self.entries.len()).map(|_| ButtonState::default()).collect();
        self.thumbs.clear();
        self.loaded = true;
    }

    /// Feed a worker `Image` response (raw file bytes) into the matching
    /// thumbnail slot, downscaling to the cell size. Called by the owning
    /// ComposeScreen when an `Image` response for one of our requested
    /// paths arrives.
    pub fn on_image(&mut self, path: &str, bytes: &Result<Vec<u8>, String>) {
        if !self.thumbs.contains_key(path) {
            return; // stale (we navigated away) or not ours
        }
        let state = match bytes {
            Ok(b) => match Texture::decode_scaled(b, THUMB_AREA_W as u32, THUMB_AREA_H as u32) {
                Ok(t) => ThumbState::Ready(t),
                Err(_) => ThumbState::Failed,
            },
            Err(_) => ThumbState::Failed,
        };
        self.thumbs.insert(path.to_string(), state);
    }

    /// Keep the cursor's row within the visible window.
    fn ensure_cursor_visible(&mut self) {
        let row = self.cursor / COLS;
        if row < self.scroll_row {
            self.scroll_row = row;
        } else if row >= self.scroll_row + VISIBLE_ROWS {
            self.scroll_row = row + 1 - VISIBLE_ROWS;
        }
    }

    /// Render + handle input for one frame. Returns `Some(..)` when the
    /// interaction is done (caller should drop the picker).
    pub fn render(&mut self, frame: &mut Frame, font: &Font, ctx: &UiCtx) -> Option<PickResult> {
        if !self.loaded {
            self.load();
        }

        // Opaque modal backdrop.
        frame.fill_rect(0.0, 0.0, SCREEN_WIDTH as f32, SCREEN_HEIGHT as f32, theme::BACKGROUND);

        // ── Shortcut chips ────────────────────────────────────────────
        let mut chip_x = MARGIN_X;
        let mut chip_nav: Option<String> = None;
        for (i, (label, root)) in SHORTCUTS.iter().enumerate() {
            let (lw, _) = frame.measure_text(font, 0.9, label);
            let cw = lw + 24;
            let rect = Rect::new(chip_x as f32, 8.0, cw as f32, 32.0);
            let active = self.path == *root;
            let bg = if active { theme::ACCENT } else { theme::FIELD_BG };
            frame.fill_rect(rect.x, rect.y, rect.w, rect.h, bg);
            frame.draw_text(font, chip_x + 12, 30, theme::TEXT_PRIMARY, 0.9, label);
            if clean_tap(rect, &mut self.chip_btns[i], ctx) {
                chip_nav = Some(root.to_string());
            }
            chip_x += cw + GAP;
        }
        if let Some(root) = chip_nav {
            self.navigate(root);
            self.load();
        }

        // ── Breadcrumb (current path) ─────────────────────────────────
        frame.draw_text(font, MARGIN_X, 68, theme::TEXT_MUTED, 0.85, &self.path);

        // L1/R1 cycle the shortcut roots (button "tab" switching). When
        // we're deep in a subfolder (not on a root), L1/R1 jump to the
        // last/first shortcut respectively.
        if ctx.pad.just_pressed(buttons::L1) || ctx.pad.just_pressed(buttons::R1) {
            let n = SHORTCUTS.len();
            let cur = SHORTCUTS.iter().position(|(_, r)| *r == self.path);
            let fwd = ctx.pad.just_pressed(buttons::R1);
            let next = match cur {
                Some(i) if fwd => (i + 1) % n,
                Some(i) => (i + n - 1) % n,
                None if fwd => 0,
                None => n - 1,
            };
            self.navigate(SHORTCUTS[next].1.to_string());
            self.load();
        }

        // ── Input: D-pad cursor + CROSS/CIRCLE ────────────────────────
        let len = self.entries.len();
        if len > 0 {
            if ctx.pad.just_pressed(buttons::UP) && self.cursor >= COLS {
                self.cursor -= COLS;
            }
            if ctx.pad.just_pressed(buttons::DOWN) && self.cursor + COLS < len {
                self.cursor += COLS;
            }
            if ctx.pad.just_pressed(buttons::LEFT) && self.cursor % COLS > 0 {
                self.cursor -= 1;
            }
            if ctx.pad.just_pressed(buttons::RIGHT)
                && self.cursor % COLS < COLS - 1
                && self.cursor + 1 < len
            {
                self.cursor += 1;
            }
            self.ensure_cursor_visible();

            if ctx.pad.just_pressed(buttons::CROSS) {
                if let Some(act) = self.activate(self.cursor) {
                    return self.apply(act);
                }
            }
        }

        // CIRCLE → up one level, or cancel at a mount root.
        if ctx.pad.just_pressed(buttons::CIRCLE) {
            match parent_dir(&self.path) {
                Some(p) => {
                    self.navigate(p);
                    self.load();
                }
                None => return Some(PickResult::Cancelled),
            }
        }

        // Request thumbnails for visible image cells we haven't asked for
        // yet. Lazy + only-visible keeps decode work and memory bounded.
        if let Some(worker) = ctx.worker {
            for idx in 0..self.entries.len() {
                let row = idx / COLS;
                if row < self.scroll_row || row >= self.scroll_row + VISIBLE_ROWS {
                    continue;
                }
                if self.entries[idx].is_dir {
                    continue;
                }
                let path = child_dir(&self.path, &self.entries[idx].name);
                if !self.thumbs.contains_key(&path) {
                    self.thumbs.insert(path.clone(), ThumbState::Loading);
                    worker.send(WorkRequest::ReadImageFile { path });
                }
            }
        }

        // ── Grid ──────────────────────────────────────────────────────
        if let Some(err) = &self.error {
            frame.draw_text_centered(font, GRID_TOP + 60, theme::ERROR, 0.95, &format!("Can't open: {err}"));
        } else if self.entries.is_empty() {
            frame.draw_text_centered(font, GRID_TOP + 60, theme::TEXT_MUTED, 0.95, "No images or folders here");
        } else {
            let mut tap_idx: Option<usize> = None;
            for idx in 0..self.entries.len() {
                let row = idx / COLS;
                if row < self.scroll_row || row >= self.scroll_row + VISIBLE_ROWS {
                    continue;
                }
                let rect = cell_rect(idx, self.scroll_row);
                self.draw_cell(frame, font, idx, rect, idx == self.cursor);
                if clean_tap(rect, &mut self.cell_btns[idx], ctx) {
                    tap_idx = Some(idx);
                }
            }
            if let Some(idx) = tap_idx {
                self.cursor = idx;
                if let Some(act) = self.activate(idx) {
                    return self.apply(act);
                }
            }
        }

        // ── Footer hint ───────────────────────────────────────────────
        frame.draw_text(
            font,
            MARGIN_X,
            SCREEN_HEIGHT - 8,
            theme::TEXT_MUTED,
            0.8,
            "X open/pick   O up/cancel   D-pad move   L1/R1 tabs",
        );

        None
    }

    /// Decide what activating entry `idx` does, as a deferred action so we
    /// don't hold an `&self.entries` borrow across a `&mut self` navigate.
    fn activate(&self, idx: usize) -> Option<CellAction> {
        let e = self.entries.get(idx)?;
        if e.is_dir {
            Some(CellAction::Enter(child_dir(&self.path, &e.name)))
        } else {
            Some(CellAction::Pick(child_dir(&self.path, &e.name)))
        }
    }

    fn apply(&mut self, act: CellAction) -> Option<PickResult> {
        match act {
            CellAction::Enter(path) => {
                self.navigate(path);
                self.load();
                None
            }
            CellAction::Pick(path) => Some(PickResult::Picked(path)),
        }
    }

    fn draw_cell(&self, frame: &mut Frame, font: &Font, idx: usize, rect: Rect, selected: bool) {
        let e = &self.entries[idx];
        // Selection highlight: ACCENT border via an inset fill.
        if selected {
            frame.fill_rect(rect.x, rect.y, rect.w, rect.h, theme::ACCENT);
            frame.fill_rect(
                rect.x + 3.0,
                rect.y + 3.0,
                rect.w - 6.0,
                rect.h - 6.0,
                theme::FIELD_BG,
            );
        } else {
            frame.fill_rect(rect.x, rect.y, rect.w, rect.h, theme::FIELD_BG);
        }

        // Thumbnail area (top portion of the cell, above the label). For
        // files this box is where step 3 draws the decoded thumbnail; for
        // now it holds a drawn folder icon or stays empty. Drawn shapes
        // (not font glyphs) so we don't depend on the TTF having emoji.
        let tx = rect.x + 6.0;
        let ty = rect.y + 6.0;
        let tw = rect.w - 12.0;
        let th = rect.h - 6.0 - 26.0;
        frame.fill_rect(tx, ty, tw, th, theme::FIELD_BG_FOCUS);
        if e.is_dir {
            // Folder glyph: a body rect + a small tab, in ACCENT.
            let iw = 64.0;
            let ih = 46.0;
            let ix = tx + (tw - iw) / 2.0;
            let iy = ty + (th - ih) / 2.0;
            frame.fill_rect(ix, iy, 28.0, 10.0, theme::ACCENT);
            frame.fill_rect(ix, iy + 8.0, iw, ih - 8.0, theme::ACCENT);
        } else {
            // Image file: draw the decoded thumbnail (centered, letterboxed
            // over the box) once ready; otherwise leave the box.
            let path = child_dir(&self.path, &e.name);
            match self.thumbs.get(&path) {
                Some(ThumbState::Ready(tex)) => {
                    let dx = tx as i32 + (THUMB_AREA_W - tex.width()) / 2;
                    let dy = ty as i32 + (THUMB_AREA_H - tex.height()) / 2;
                    frame.draw_texture(tex, dx as f32, dy as f32);
                }
                Some(ThumbState::Failed) => {
                    let (mw, _) = frame.measure_text(font, 1.0, "?");
                    frame.draw_text(
                        font,
                        tx as i32 + (THUMB_AREA_W - mw) / 2,
                        ty as i32 + THUMB_AREA_H / 2,
                        theme::TEXT_MUTED,
                        1.0,
                        "?",
                    );
                }
                _ => {} // Loading / not yet requested: leave the box
            }
        }

        // Filename label, truncated to the cell width.
        let label = truncate_to_width(frame, font, 0.8, &e.name, rect.w as i32 - 12);
        let (lw, _) = frame.measure_text(font, 0.8, &label);
        frame.draw_text(
            font,
            rect.x as i32 + (rect.w as i32 - lw) / 2,
            rect.y as i32 + rect.h as i32 - 12,
            theme::TEXT_MUTED,
            0.8,
            &label,
        );
    }
}

impl Default for FilePicker {
    fn default() -> Self {
        Self::new()
    }
}

enum CellAction {
    Enter(String),
    Pick(String),
}

fn cell_rect(idx: usize, scroll_row: usize) -> Rect {
    let row = idx / COLS;
    let col = idx % COLS;
    let screen_row = row - scroll_row;
    let x = MARGIN_X + col as i32 * (CELL_W + GAP);
    let y = GRID_TOP + screen_row as i32 * (CELL_H + GAP);
    Rect::new(x as f32, y as f32, CELL_W as f32, CELL_H as f32)
}

fn is_image(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".jpg") || l.ends_with(".jpeg") || l.ends_with(".png")
}

/// Parent of a Sce path. `ux0:` → None (mount root); `ux0:data` → `ux0:`;
/// `ux0:a/b` → `ux0:a`.
fn parent_dir(path: &str) -> Option<String> {
    if let Some(idx) = path.rfind('/') {
        let p = &path[..idx];
        if !p.is_empty() {
            return Some(p.to_string());
        }
    }
    // No slash: `drive:sub` → `drive:`; `drive:` → None.
    if let Some(cidx) = path.find(':') {
        if cidx + 1 < path.len() {
            return Some(path[..=cidx].to_string());
        }
    }
    None
}

/// Child path under `path`. Roots end in `:` (no separator), otherwise a
/// `/` separator is inserted.
fn child_dir(path: &str, name: &str) -> String {
    if path.ends_with(':') || path.ends_with('/') {
        format!("{path}{name}")
    } else {
        format!("{path}/{name}")
    }
}

/// Truncate `text` with a trailing `…` so it fits in `max_w` pixels.
fn truncate_to_width(frame: &Frame, font: &Font, scale: f32, text: &str, max_w: i32) -> String {
    if frame.measure_text(font, scale, text).0 <= max_w {
        return text.to_string();
    }
    let mut s = String::new();
    for ch in text.chars() {
        let cand = format!("{s}{ch}…");
        if frame.measure_text(font, scale, &cand).0 > max_w {
            break;
        }
        s.push(ch);
    }
    s.push('…');
    s
}

/// Clean tap (press-inside then release with no touches) over `rect`.
fn clean_tap(rect: Rect, state: &mut ButtonState, ctx: &UiCtx) -> bool {
    let pressed_now = ctx.touches.iter().any(|t| rect.contains(t.x, t.y));
    let clicked = state.pressed_last && !pressed_now && ctx.touches.is_empty();
    state.pressed_last = pressed_now;
    clicked
}

//! Post-embed rendering — images, link cards, quote posts, video.
//!
//! Hooked into `draw_post_row` / `measure_post_row`. The same module
//! services TimelineScreen, ThreadScreen, SearchScreen, and any custom
//! feed since they all funnel through the shared row helpers.
//!
//! Layout (vertically): `body_text → EMBED_GAP → embed_block →
//! BODY_GAP → counts_row`. The embed block is `0` when `post.embed` is
//! `None`, so posts without embeds keep the original tight layout.
//!
//! Embed variants:
//!
//! - **images** (1–4): aspect-fit single image (max 320 px tall) or a
//!   stretch-to-fill grid (2/3 in a row at 240 px, 4 in 2×2 at 180 px).
//! - **external**: 60×60 thumb + title + URL host card (fixed 76 px).
//! - **record** (quote post): bordered card with quoted author + body.
//!   Tap → ThreadScreen of the quoted post. `ViewBlocked` /
//!   `ViewNotFound` / `ViewDetached` and non-post variants render a
//!   muted "Post unavailable" placeholder (no tap).
//! - **video**: image-style thumbnail with a centered ▶ overlay
//!   (no playback in 5.2 — that's 5.3).
//! - **recordWithMedia**: media block above the quote card.

use atrium_api::app::bsky::embed::defs::AspectRatio;
use atrium_api::app::bsky::embed::external::View as ExternalView;
use atrium_api::app::bsky::embed::images::View as ImagesView;
use atrium_api::app::bsky::embed::record::{View as RecordView, ViewRecord, ViewRecordRefs};
use atrium_api::app::bsky::embed::record_with_media::ViewMediaRefs;
use atrium_api::app::bsky::embed::video::View as VideoView;
use atrium_api::app::bsky::feed::defs::{FeedViewPost, PostViewEmbedRefs};
use atrium_api::types::Union;
use bsky_render::{theme, Color, EmojiAtlas, Font, Frame, TextureCache};

use crate::timeline::{extract_post_text, ROW_PAD_X, ROW_PAD_Y, TEXT_LEFT, TOP_LINE_H};

/// Vertical gap between body text and the embed block.
pub(crate) const EMBED_GAP: i32 = 8;
/// Vertical gap between the embed block and the counts row, used
/// instead of `BODY_GAP` whenever a row has an embed. The extra
/// breathing room makes images feel less cramped against the
/// likes / reposts / replies labels.
pub(crate) const EMBED_BOTTOM_GAP: i32 = 16;
/// Hard cap on the entire embed block's height — keeps oversized
/// embeds from blowing up the post row.
pub(crate) const EMBED_MAX_H: i32 = 368;

const SCREEN_WIDTH: i32 = bsky_render::SCREEN_WIDTH;
/// Embed block inner width = post-row inner width.
const INNER_W: i32 = SCREEN_WIDTH - TEXT_LEFT - ROW_PAD_X;

const GRID_GAP: i32 = 8;
const SINGLE_IMG_MAX_H: i32 = 320;
const GRID_ROW_H: i32 = 240; // for n=2,3
const GRID_4_CELL_H: i32 = 180;
const GRID_4_TOTAL_H: i32 = GRID_4_CELL_H * 2 + GRID_GAP; // 368
const EXTERNAL_CARD_H: i32 = 76;
const EXTERNAL_THUMB: i32 = 60;
/// Compact "this post has a video" placeholder. We don't render the
/// video's still-frame thumbnail — just a small dark rect with a
/// centered ▶ glyph, indicating playback is available (in 5.3).
const VIDEO_PLACEHOLDER_W: i32 = 100;
const VIDEO_PLACEHOLDER_H: i32 = 60;
const QUOTE_PAD: i32 = 8;
const QUOTE_AVATAR: i32 = 24;
/// Vertical span from the card's top to the first body baseline.
/// QUOTE_PAD (8) + avatar height (24) leaves the avatar's bottom at
/// the line where body line 1's ascender would otherwise crash into
/// the avatar; +20 puts the body baseline well clear, accounting for
/// Inter's ~18 px ascender at scale 0.95.
const QUOTE_HEADER_H: i32 = QUOTE_AVATAR + 20;
const PLACEHOLDER_QUOTE_H: i32 = 40;

/// Approximate aspect (width/height) for an image — falls back to 16:9
/// when the embed didn't come with explicit dimensions.
fn aspect_or_default(ar: Option<&AspectRatio>) -> f32 {
    ar.map(|a| a.width.get() as f32 / a.height.get() as f32)
        .unwrap_or(1.7777)
}

// ─── Public API ────────────────────────────────────────────────────────

/// Total embed-block height (including the leading EMBED_GAP). Returns
/// 0 if the post has no embed.
pub(crate) fn measure_embed_block(
    frame: &Frame,
    font: &Font,
    embed: Option<&Union<PostViewEmbedRefs>>,
    emoji: Option<&EmojiAtlas>,
) -> i32 {
    let Some(Union::Refs(refs)) = embed else {
        return 0;
    };
    let h = match refs {
        PostViewEmbedRefs::AppBskyEmbedImagesView(v) => {
            let first_ar = v.images.first().and_then(|i| i.aspect_ratio.as_ref());
            measure_image_grid(v.images.len(), first_ar)
        }
        PostViewEmbedRefs::AppBskyEmbedExternalView(_) => EXTERNAL_CARD_H,
        PostViewEmbedRefs::AppBskyEmbedVideoView(_) => VIDEO_PLACEHOLDER_H,
        PostViewEmbedRefs::AppBskyEmbedRecordView(v) => {
            measure_record_view(frame, font, v, emoji)
        }
        PostViewEmbedRefs::AppBskyEmbedRecordWithMediaView(v) => {
            let media_h = measure_media(frame, font, &v.media);
            let quote_h = measure_record_view(frame, font, &v.record, emoji);
            media_h + GRID_GAP + quote_h
        }
    };
    h.min(EMBED_MAX_H) + EMBED_GAP
}

/// Render the embed block at `(x, y)`. Caller has already advanced past
/// the leading `EMBED_GAP` — that gap belongs to the row's vertical
/// budget, not to `draw_embed_block`.
pub(crate) fn draw_embed_block(
    frame: &mut Frame,
    font: &Font,
    embed: &Union<PostViewEmbedRefs>,
    x: i32,
    y: i32,
    cache: &TextureCache,
    emoji: Option<&EmojiAtlas>,
) {
    let Union::Refs(refs) = embed else {
        return;
    };
    match refs {
        PostViewEmbedRefs::AppBskyEmbedImagesView(v) => {
            draw_image_grid(frame, x, y, v, cache);
        }
        PostViewEmbedRefs::AppBskyEmbedExternalView(v) => {
            draw_external_card(frame, font, x, y, v, cache);
        }
        PostViewEmbedRefs::AppBskyEmbedVideoView(v) => {
            draw_video_thumb(frame, x, y, v, cache);
        }
        PostViewEmbedRefs::AppBskyEmbedRecordView(v) => {
            draw_record_view(frame, font, x, y, v, cache, emoji);
        }
        PostViewEmbedRefs::AppBskyEmbedRecordWithMediaView(v) => {
            let media_h = measure_media(frame, font, &v.media);
            draw_media(frame, font, x, y, &v.media, cache);
            draw_record_view(frame, font, x, y + media_h + GRID_GAP, &v.record, cache, emoji);
        }
    }
}

/// All CDN URLs the embed needs to display. The avatar dispatch loop
/// in TimelineScreen feeds these into `WorkRequest::FetchImage`.
pub(crate) fn embed_image_urls(
    embed: Option<&Union<PostViewEmbedRefs>>,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(Union::Refs(refs)) = embed else {
        return out;
    };
    match refs {
        PostViewEmbedRefs::AppBskyEmbedImagesView(v) => {
            for img in v.images.iter() {
                out.push(crate::cdn::ensure_jpeg(&img.thumb));
            }
        }
        PostViewEmbedRefs::AppBskyEmbedExternalView(v) => {
            if let Some(t) = v.external.thumb.as_ref() {
                out.push(crate::cdn::ensure_jpeg(t));
            }
        }
        PostViewEmbedRefs::AppBskyEmbedVideoView(_) => {
            // No thumbnail fetch — we render only a play-button placeholder.
        }
        PostViewEmbedRefs::AppBskyEmbedRecordView(v) => {
            collect_record_urls(&v.record, &mut out);
        }
        PostViewEmbedRefs::AppBskyEmbedRecordWithMediaView(v) => {
            collect_media_urls(&v.media, &mut out);
            collect_record_urls(&v.record.record, &mut out);
        }
    }
    out
}

/// One image for the full-screen viewer: CDN URLs (jpeg-coerced) + alt.
#[derive(Clone, Debug)]
pub struct ViewerImage {
    pub thumb: String,
    pub fullsize: String,
    pub alt: String,
}

/// If the post's embed is a plain images embed, return its viewer images
/// paired with each cell's absolute screen rect `(x, y, w, h)` — mirrors
/// `draw_image_grid`'s n=1..4 layout so a tap maps to the right image.
/// (recordWithMedia images are out of scope for v1.)
pub(crate) fn image_tap_cells(
    frame: &Frame,
    font: &Font,
    post: &FeedViewPost,
    row_top: i32,
    emoji: Option<&EmojiAtlas>,
) -> Option<(Vec<ViewerImage>, Vec<(i32, i32, i32, i32)>)> {
    let embed = post.post.embed.as_ref()?;
    let Union::Refs(refs) = embed else { return None };
    let PostViewEmbedRefs::AppBskyEmbedImagesView(v) = refs else {
        return None;
    };
    let n = v.images.len();
    if n == 0 {
        return None;
    }
    let (ey, _eh) = embed_rect(frame, font, post, row_top, emoji)?;
    let x = TEXT_LEFT;
    let mut rects: Vec<(i32, i32, i32, i32)> = Vec::with_capacity(n);
    match n {
        1 => {
            let img = &v.images[0];
            let h = measure_aspect_image(img.aspect_ratio.as_ref());
            let aspect = aspect_or_default(img.aspect_ratio.as_ref());
            let w = ((h as f32 * aspect) as i32).min(INNER_W);
            rects.push((x, ey, w, h));
        }
        2 => {
            let cw = (INNER_W - GRID_GAP) / 2;
            rects.push((x, ey, cw, GRID_ROW_H));
            rects.push((x + cw + GRID_GAP, ey, cw, GRID_ROW_H));
        }
        3 => {
            let cw = (INNER_W - GRID_GAP * 2) / 3;
            for i in 0..3 {
                rects.push((x + i * (cw + GRID_GAP), ey, cw, GRID_ROW_H));
            }
        }
        _ => {
            let cw = (INNER_W - GRID_GAP) / 2;
            for i in 0..4 {
                let row = i / 2;
                let col = i % 2;
                rects.push((
                    x + col * (cw + GRID_GAP),
                    ey + row * (GRID_4_CELL_H + GRID_GAP),
                    cw,
                    GRID_4_CELL_H,
                ));
            }
        }
    }
    let images = v
        .images
        .iter()
        .take(rects.len())
        .map(|img| ViewerImage {
            thumb: crate::cdn::ensure_jpeg(&img.thumb),
            fullsize: crate::cdn::ensure_jpeg(&img.fullsize),
            alt: img.alt.clone(),
        })
        .collect();
    Some((images, rects))
}

/// (`did`, `cid`) target for a tappable video embed, or `None` when
/// the embed isn't (or doesn't contain) a video. `did` is the post
/// author's DID — `getBlob` needs it to route to the right repo.
#[derive(Clone, Debug)]
pub struct VideoTarget {
    pub did: String,
    pub cid: String,
}

pub(crate) fn video_in_embed(
    embed: Option<&Union<PostViewEmbedRefs>>,
    author_did: &str,
) -> Option<VideoTarget> {
    let Some(Union::Refs(refs)) = embed else {
        return None;
    };
    let cid = match refs {
        PostViewEmbedRefs::AppBskyEmbedVideoView(v) => v.cid.as_ref().to_string(),
        PostViewEmbedRefs::AppBskyEmbedRecordWithMediaView(v) => match v.media.into_refs() {
            Some(ViewMediaRefs::AppBskyEmbedVideoView(vv)) => vv.cid.as_ref().to_string(),
            _ => return None,
        },
        _ => return None,
    };
    Some(VideoTarget {
        did: author_did.to_string(),
        cid,
    })
}

/// AT-URI of the quoted post if the embed contains one. Returns
/// `None` for non-quote embeds and for unavailable quotes
/// (blocked / not found / detached / non-post records).
pub(crate) fn quote_uri_in_embed(
    embed: Option<&Union<PostViewEmbedRefs>>,
) -> Option<String> {
    let Some(Union::Refs(refs)) = embed else {
        return None;
    };
    match refs {
        PostViewEmbedRefs::AppBskyEmbedRecordView(v) => quote_uri_from_record_view(v),
        PostViewEmbedRefs::AppBskyEmbedRecordWithMediaView(v) => {
            quote_uri_from_record_view(&v.record)
        }
        _ => None,
    }
}

/// Convenience: the (y, h) absolute coords of the embed block within a
/// post row. Used by tap detection to refine the body-tap zone.
pub(crate) fn embed_rect(
    frame: &Frame,
    font: &Font,
    post: &FeedViewPost,
    row_top: i32,
    emoji: Option<&EmojiAtlas>,
) -> Option<(i32, i32)> {
    let embed = post.post.embed.as_ref()?;
    let body_text = extract_post_text(&post.post.record).unwrap_or_default();
    let body_h = frame.measure_text_wrapped_with_emoji(font, INNER_W, 1.0, &body_text, emoji);
    let block = measure_embed_block(frame, font, Some(embed), emoji);
    if block == 0 {
        return None;
    }
    let embed_y = row_top + ROW_PAD_Y + TOP_LINE_H + body_h + EMBED_GAP;
    let embed_h = (block - EMBED_GAP).max(0);
    Some((embed_y, embed_h))
}

// ─── Trait helper to peel a Union<Refs> ────────────────────────────────

trait UnionRefsExt<T> {
    fn into_refs(&self) -> Option<&T>;
}

impl<T> UnionRefsExt<T> for Union<T> {
    fn into_refs(&self) -> Option<&T> {
        match self {
            Union::Refs(r) => Some(r),
            Union::Unknown(_) => None,
        }
    }
}

// ─── Image grid ────────────────────────────────────────────────────────

fn measure_image_grid(n: usize, first_aspect: Option<&AspectRatio>) -> i32 {
    match n {
        0 => 0,
        1 => measure_aspect_image(first_aspect),
        2 | 3 => GRID_ROW_H,
        _ => GRID_4_TOTAL_H,
    }
}

fn measure_aspect_image(ar: Option<&AspectRatio>) -> i32 {
    let aspect = aspect_or_default(ar);
    let h_from_w = (INNER_W as f32 / aspect) as i32;
    h_from_w.clamp(80, SINGLE_IMG_MAX_H)
}

fn draw_image_grid(
    frame: &mut Frame,
    x: i32,
    y: i32,
    v: &ImagesView,
    cache: &TextureCache,
) {
    let n = v.images.len();
    if n == 0 {
        return;
    }
    if n == 1 {
        let img = &v.images[0];
        let h = measure_aspect_image(img.aspect_ratio.as_ref());
        let aspect = aspect_or_default(img.aspect_ratio.as_ref());
        let w = (h as f32 * aspect) as i32;
        let w = w.min(INNER_W);
        // Left-justified — flush with the body text column.
        draw_image_cell(frame, &crate::cdn::ensure_jpeg(&img.thumb), x, y, w, h, cache, true);
        return;
    }
    if n == 2 {
        let cell_w = (INNER_W - GRID_GAP) / 2;
        draw_image_cell(frame, &v.images[0].thumb, x, y, cell_w, GRID_ROW_H, cache, false);
        draw_image_cell(
            frame,
            &v.images[1].thumb,
            x + cell_w + GRID_GAP,
            y,
            cell_w,
            GRID_ROW_H,
            cache,
            false,
        );
        return;
    }
    if n == 3 {
        let cell_w = (INNER_W - GRID_GAP * 2) / 3;
        for (i, img) in v.images.iter().enumerate() {
            let cx = x + (i as i32) * (cell_w + GRID_GAP);
            draw_image_cell(frame, &crate::cdn::ensure_jpeg(&img.thumb), cx, y, cell_w, GRID_ROW_H, cache, false);
        }
        return;
    }
    // n >= 4 → 2×2.
    let cell_w = (INNER_W - GRID_GAP) / 2;
    for (i, img) in v.images.iter().take(4).enumerate() {
        let row = (i / 2) as i32;
        let col = (i % 2) as i32;
        let cx = x + col * (cell_w + GRID_GAP);
        let cy = y + row * (GRID_4_CELL_H + GRID_GAP);
        draw_image_cell(frame, &crate::cdn::ensure_jpeg(&img.thumb), cx, cy, cell_w, GRID_4_CELL_H, cache, false);
    }
}

fn draw_image_cell(
    frame: &mut Frame,
    thumb_url: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    cache: &TextureCache,
    aspect_fit: bool,
) {
    if let Some(tex) = cache.get(thumb_url) {
        let tw = tex.width().max(1) as f32;
        let th = tex.height().max(1) as f32;
        if aspect_fit {
            let s = (w as f32 / tw).min(h as f32 / th);
            let dw = tw * s;
            let dh = th * s;
            let dx = x as f32 + (w as f32 - dw) / 2.0;
            let dy = y as f32 + (h as f32 - dh) / 2.0;
            // Letterbox background.
            frame.fill_rect(x as f32, y as f32, w as f32, h as f32, theme::FIELD_BG);
            frame.draw_texture_scale(tex, dx, dy, s, s);
        } else {
            let sx = w as f32 / tw;
            let sy = h as f32 / th;
            frame.draw_texture_scale(tex, x as f32, y as f32, sx, sy);
        }
    } else {
        // Placeholder while loading.
        frame.fill_rect(
            x as f32,
            y as f32,
            w as f32,
            h as f32,
            theme::FIELD_BG,
        );
    }
}

// ─── External link card ───────────────────────────────────────────────

fn draw_external_card(
    frame: &mut Frame,
    font: &Font,
    x: i32,
    y: i32,
    v: &ExternalView,
    cache: &TextureCache,
) {
    // Card outline + bg.
    frame.fill_rect(x as f32, y as f32, INNER_W as f32, EXTERNAL_CARD_H as f32, theme::FIELD_BG);
    let pad = 8;
    let thumb_x = x + pad;
    let thumb_y = y + pad;
    if let Some(turl) = v.external.thumb.as_deref() {
        let turl = crate::cdn::ensure_jpeg(turl);
        if let Some(tex) = cache.get(&turl) {
            let sx = EXTERNAL_THUMB as f32 / tex.width().max(1) as f32;
            let sy = EXTERNAL_THUMB as f32 / tex.height().max(1) as f32;
            frame.draw_texture_scale(tex, thumb_x as f32, thumb_y as f32, sx, sy);
        } else {
            frame.fill_rect(
                thumb_x as f32,
                thumb_y as f32,
                EXTERNAL_THUMB as f32,
                EXTERNAL_THUMB as f32,
                theme::BACKGROUND,
            );
        }
    } else {
        frame.fill_rect(
            thumb_x as f32,
            thumb_y as f32,
            EXTERNAL_THUMB as f32,
            EXTERNAL_THUMB as f32,
            theme::BACKGROUND,
        );
    }
    let text_x = thumb_x + EXTERNAL_THUMB + 8;
    let text_w = INNER_W - (text_x - x) - pad;
    let title = truncate_to_width(frame, font, &v.external.title, 1.0, text_w);
    frame.draw_text(font, text_x, y + pad + 18, theme::TEXT_PRIMARY, 1.0, &title);
    let host = host_from_uri(&v.external.uri);
    let host_t = truncate_to_width(frame, font, &host, 0.85, text_w);
    frame.draw_text(font, text_x, y + pad + 44, theme::TEXT_MUTED, 0.85, &host_t);
}

fn host_from_uri(uri: &str) -> String {
    if let Some(after_scheme) = uri.split_once("://") {
        let rest = after_scheme.1;
        let host = rest.split('/').next().unwrap_or(rest);
        return host.to_string();
    }
    uri.chars().take(64).collect()
}

// ─── Quote post (record view) ─────────────────────────────────────────

fn measure_record_view(
    frame: &Frame,
    font: &Font,
    v: &RecordView,
    emoji: Option<&EmojiAtlas>,
) -> i32 {
    match v.record.into_refs() {
        Some(ViewRecordRefs::ViewRecord(r)) => measure_quote_card(frame, font, r, emoji),
        Some(_) | None => PLACEHOLDER_QUOTE_H,
    }
}

fn measure_quote_card(
    frame: &Frame,
    font: &Font,
    r: &ViewRecord,
    emoji: Option<&EmojiAtlas>,
) -> i32 {
    let body_text = extract_post_text(&r.value).unwrap_or_default();
    let inner_text_w = INNER_W - QUOTE_PAD * 2;
    let body_h = frame.measure_text_wrapped_with_emoji(font, inner_text_w, 0.95, &body_text, emoji);
    QUOTE_PAD + QUOTE_HEADER_H + body_h + QUOTE_PAD
}

fn draw_record_view(
    frame: &mut Frame,
    font: &Font,
    x: i32,
    y: i32,
    v: &RecordView,
    cache: &TextureCache,
    emoji: Option<&EmojiAtlas>,
) {
    match v.record.into_refs() {
        Some(ViewRecordRefs::ViewRecord(r)) => {
            draw_quote_card(frame, font, x, y, r, cache, emoji);
        }
        Some(_) | None => draw_placeholder_card(frame, font, x, y, "Post unavailable"),
    }
}

fn draw_placeholder_card(frame: &mut Frame, font: &Font, x: i32, y: i32, text: &str) {
    draw_card_border(frame, x, y, INNER_W, PLACEHOLDER_QUOTE_H);
    frame.draw_text(
        font,
        x + QUOTE_PAD,
        y + 24,
        theme::TEXT_MUTED,
        0.95,
        text,
    );
}

fn draw_quote_card(
    frame: &mut Frame,
    font: &Font,
    x: i32,
    y: i32,
    r: &ViewRecord,
    cache: &TextureCache,
    emoji: Option<&EmojiAtlas>,
) {
    let body_text = extract_post_text(&r.value).unwrap_or_default();
    let inner_text_w = INNER_W - QUOTE_PAD * 2;
    let body_h = frame.measure_text_wrapped_with_emoji(font, inner_text_w, 0.95, &body_text, emoji);
    let total_h = QUOTE_PAD + QUOTE_HEADER_H + body_h + QUOTE_PAD;

    draw_card_border(frame, x, y, INNER_W, total_h);

    // Quote header: 24×24 avatar + display name + @handle on one line.
    let avatar_x = x + QUOTE_PAD;
    let avatar_y = y + QUOTE_PAD;
    let handle_str = r.author.handle.as_str();
    if let Some(url) = r.author.avatar.as_deref() {
        // Quote avatars use the avatar URL directly (Phase 5.1 / 4.x
        // already dispatch them through `embed_image_urls` →
        // FetchImage). Skip the avatar_thumbnail_jpeg transform: it
        // expects /avatar/plain/ which embed-collected URLs already
        // satisfy when written by the post author client.
        let url = crate::cdn::avatar_thumbnail_jpeg(url);
        if let Some(tex) = cache.get(&url) {
            let sx = QUOTE_AVATAR as f32 / tex.width().max(1) as f32;
            let sy = QUOTE_AVATAR as f32 / tex.height().max(1) as f32;
            frame.draw_texture_scale(tex, avatar_x as f32, avatar_y as f32, sx, sy);
        } else {
            frame.fill_rect(
                avatar_x as f32,
                avatar_y as f32,
                QUOTE_AVATAR as f32,
                QUOTE_AVATAR as f32,
                placeholder_color(handle_str),
            );
        }
    } else {
        frame.fill_rect(
            avatar_x as f32,
            avatar_y as f32,
            QUOTE_AVATAR as f32,
            QUOTE_AVATAR as f32,
            placeholder_color(handle_str),
        );
    }

    let display = r
        .author
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(handle_str);
    let header_x = avatar_x + QUOTE_AVATAR + 6;
    let header_y = avatar_y + 16;
    let display_truncated =
        truncate_to_width(frame, font, display, 0.95, INNER_W / 2);
    let dw = frame.measure_text(font, 0.95, &display_truncated).0;
    frame.draw_text(font, header_x, header_y, theme::TEXT_PRIMARY, 0.95, &display_truncated);
    let handle_label = format!("@{handle_str}");
    let handle_x = header_x + dw + 6;
    let remaining = INNER_W - (handle_x - x) - QUOTE_PAD;
    if remaining > 30 {
        let handle_truncated =
            truncate_to_width(frame, font, &handle_label, 0.85, remaining);
        frame.draw_text(font, handle_x, header_y, theme::TEXT_MUTED, 0.85, &handle_truncated);
    }

    // Body text.
    let body_x = x + QUOTE_PAD;
    let body_y = y + QUOTE_PAD + QUOTE_HEADER_H;
    frame.draw_text_wrapped_with_emoji(
        font,
        body_x,
        body_y,
        inner_text_w,
        theme::TEXT_PRIMARY,
        0.95,
        &body_text,
        emoji,
    );
}

fn quote_uri_from_record_view(v: &RecordView) -> Option<String> {
    match v.record.into_refs()? {
        ViewRecordRefs::ViewRecord(r) => Some(r.uri.clone()),
        _ => None,
    }
}

fn collect_record_urls(record: &Union<ViewRecordRefs>, out: &mut Vec<String>) {
    if let Some(ViewRecordRefs::ViewRecord(r)) = record.into_refs() {
        if let Some(url) = r.author.avatar.as_deref() {
            // Mirror the transform `draw_quote_card` will use.
            out.push(crate::cdn::avatar_thumbnail_jpeg(url));
        }
    }
}

// ─── Video thumbnail ──────────────────────────────────────────────────

fn draw_video_thumb(
    frame: &mut Frame,
    x: i32,
    y: i32,
    _v: &VideoView,
    _cache: &TextureCache,
) {
    // Compact "video here" placeholder — no still-frame preview, just
    // a small dark rect with a centered ▶ glyph. 5.3 will hang
    // playback off the same area.
    let w = VIDEO_PLACEHOLDER_W;
    let h = VIDEO_PLACEHOLDER_H;
    let img_x = x; // left-justified
    frame.fill_rect(img_x as f32, y as f32, w as f32, h as f32, theme::FIELD_BG);

    // ▶ glyph: 24-px ACCENT circle with a triangle inside, centered.
    let glyph = 24;
    let cx = img_x + w / 2;
    let cy = y + h / 2;
    let r = glyph / 2;
    frame.fill_rect(
        (cx - r) as f32,
        (cy - r) as f32,
        glyph as f32,
        glyph as f32,
        theme::ACCENT,
    );
    // Stylized play triangle: 1-px columns whose heights taper from
    // glyph-height down to 0. vita2d has no polygon primitive.
    let tri_color = theme::TEXT_PRIMARY;
    let span = glyph / 2; // 12-px wide triangle inside the 24-px circle
    for i in 0..span {
        let bw = (span - i) * 2;
        let bx = cx - span / 2 + i;
        let by = cy - bw / 2;
        if bw > 0 {
            frame.fill_rect(bx as f32, by as f32, 1.0, bw as f32, tri_color);
        }
    }
}

// ─── recordWithMedia: render the inner media block ────────────────────

fn measure_media(frame: &Frame, font: &Font, media: &Union<ViewMediaRefs>) -> i32 {
    let _ = font;
    let _ = frame;
    match media.into_refs() {
        Some(ViewMediaRefs::AppBskyEmbedImagesView(v)) => {
            let first_ar = v.images.first().and_then(|i| i.aspect_ratio.as_ref());
            measure_image_grid(v.images.len(), first_ar)
        }
        Some(ViewMediaRefs::AppBskyEmbedVideoView(_)) => VIDEO_PLACEHOLDER_H,
        Some(ViewMediaRefs::AppBskyEmbedExternalView(_)) => EXTERNAL_CARD_H,
        None => 0,
    }
}

fn draw_media(
    frame: &mut Frame,
    font: &Font,
    x: i32,
    y: i32,
    media: &Union<ViewMediaRefs>,
    cache: &TextureCache,
) {
    match media.into_refs() {
        Some(ViewMediaRefs::AppBskyEmbedImagesView(v)) => {
            draw_image_grid(frame, x, y, v, cache);
        }
        Some(ViewMediaRefs::AppBskyEmbedVideoView(v)) => {
            draw_video_thumb(frame, x, y, v, cache);
        }
        Some(ViewMediaRefs::AppBskyEmbedExternalView(v)) => {
            draw_external_card(frame, font, x, y, v, cache);
        }
        None => {}
    }
}

fn collect_media_urls(media: &Union<ViewMediaRefs>, out: &mut Vec<String>) {
    match media.into_refs() {
        Some(ViewMediaRefs::AppBskyEmbedImagesView(v)) => {
            for img in v.images.iter() {
                out.push(crate::cdn::ensure_jpeg(&img.thumb));
            }
        }
        Some(ViewMediaRefs::AppBskyEmbedVideoView(_)) => {
            // No thumbnail fetch — we render only a play-button placeholder.
        }
        Some(ViewMediaRefs::AppBskyEmbedExternalView(v)) => {
            if let Some(t) = v.external.thumb.as_ref() {
                out.push(crate::cdn::ensure_jpeg(t));
            }
        }
        None => {}
    }
}

// ─── Shared chrome ────────────────────────────────────────────────────

fn draw_card_border(frame: &mut Frame, x: i32, y: i32, w: i32, h: i32) {
    let c = theme::FIELD_BG;
    // 4 thin rectangles forming an outline.
    frame.fill_rect(x as f32, y as f32, w as f32, 1.0, c);
    frame.fill_rect(x as f32, (y + h - 1) as f32, w as f32, 1.0, c);
    frame.fill_rect(x as f32, y as f32, 1.0, h as f32, c);
    frame.fill_rect((x + w - 1) as f32, y as f32, 1.0, h as f32, c);
}

fn truncate_to_width(
    frame: &Frame,
    font: &Font,
    text: &str,
    scale: f32,
    max_w: i32,
) -> String {
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

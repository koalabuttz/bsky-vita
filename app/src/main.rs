//! bsky-vita: PS Vita homebrew Bluesky client.
//!
//! Phase 4.1: navigation stack + tab bar + selection model.
//!
//! - `LoginScreen` (initial) checks for `session.json` on its first
//!   frame; resumes if possible, otherwise shows the login form. Tap
//!   Login → "Authenticating…" → emits `ScreenAction::AuthComplete`,
//!   carrying the `Arc<AuthClient>` we use to spawn the worker thread
//!   and retain for top-level screen construction (`SwitchTab`).
//! - Top-level screens (TimelineScreen, ProfileScreen-of-self,
//!   NotificationsScreen, SearchScreen) render the bottom tab bar and
//!   live in the `screen_stack`. Tab tap → `SwitchTab(target)` →
//!   stack truncates to (or pushes a fresh) instance of that target.
//! - Sub-screens (Compose, Thread, ProfileScreen-of-other) push onto
//!   the stack; CIRCLE pops them.
//!
//! Network calls happen on a background worker thread; the render loop
//! drains responses via `worker.try_recv()` each frame and never
//! blocks. `Screen::after_present` survives for the pre-worker
//! LoginScreen path (resume / login) — those run *before* the worker
//! exists.

use std::sync::Arc;

use bsky_auth::AuthClient;
use bsky_input::{Pad, Touch};
use bsky_render::{EmojiAtlas, Render, Texture, TextureCache};
use bsky_ui::{
    ConversationListScreen, LoginScreen, NotificationsScreen, ProfileScreen, Screen, ScreenAction,
    SearchScreen, TimelineScreen, TopLevel, UiCtx,
};
use bsky_worker::{WorkResponse, Worker};

fn main() {
    bsky_log::init("ux0:/data/BSKY00001/run.log");
    // Grant access to the OS photo gallery (ux0:picture/ albums) — without
    // this the sandbox hides CAMERA/SCREENSHOT/etc. Uses a 'static
    // mount-point string (the mount retains the pointer).
    bsky_media::fs::mount_photo_gallery();
    let mut render = match Render::init() {
        Ok(r) => r,
        Err(e) => {
            bsky_log::log!("vita2d_init failed: {e}; sleeping forever");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    };
    render.set_clear_color(bsky_render::theme::BACKGROUND);
    let font = match render.load_inter_ttf("app0:Inter-Regular.ttf") {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "Inter TTF load failed ({e}); falling back to PGF — \
                 add app/static/Inter-Regular.ttf and rebuild for crisp text"
            );
            render
                .load_default_pgf()
                .expect("PGF fallback also failed; can't render text")
        }
    };

    let emoji_atlas: Option<EmojiAtlas> = match EmojiAtlas::from_path("app0:twemoji.png") {
        Ok(a) => Some(a),
        Err(e) => {
            eprintln!(
                "Twemoji atlas load failed ({e}); emoji codepoints will render \
                 as TTF fallback. Run 'make push-emoji' to upload twemoji.png."
            );
            None
        }
    };

    let avatar_mask: Option<Texture> = match Texture::from_png_file("app0:avatar_mask_96.png") {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("avatar mask load failed ({e}); avatars will render as squares");
            None
        }
    };
    let avatar_mask_field: Option<Texture> =
        match Texture::from_png_file("app0:avatar_mask_field_96.png") {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "avatar mask (field-bg) load failed ({e}); selected-row avatars will \
                     render as squares"
                );
                None
            }
        };

    let mut pad = Pad::init();
    let touch = Touch::init();
    let mut ime = bsky_ime::Ime::new();

    // Navigation stack. Top of the stack is the currently-rendered
    // screen. LoginScreen is the initial root; AuthComplete clears the
    // stack and pushes the post-auth top-level screen.
    let mut screen_stack: Vec<Box<dyn Screen>> = vec![Box::new(LoginScreen::new())];

    // Worker is spawned the first time a screen returns `AuthComplete`.
    let mut worker: Option<Worker> = None;
    // Held alongside the worker so we can construct fresh top-level
    // screens (TimelineScreen, ProfileScreen, …) when a `SwitchTab`
    // action lands on a tab without an existing instance in the stack.
    let mut auth_client: Option<Arc<AuthClient>> = None;

    let mut texture_cache = TextureCache::new(64);

    loop {
        let pf = pad.poll();
        let tf = touch.poll();
        let ctx = UiCtx {
            touches: &tf.points,
            pad: &pf,
            worker: worker.as_ref(),
            emoji: emoji_atlas.as_ref(),
            texture_cache: &texture_cache,
            avatar_mask: avatar_mask.as_ref(),
            avatar_mask_field: avatar_mask_field.as_ref(),
        };

        // Render + collect transition action. The Frame's Drop happens
        // when this block ends, which presents the buffer.
        let action = {
            let top = screen_stack
                .last_mut()
                .expect("screen stack is never empty");
            let mut frame = render.begin_frame();
            let action = top.frame(&mut frame, &font, &ctx, &mut ime);
            if ime.is_active() {
                frame.pump_ime();
            }
            action
        };

        // Pre-worker blocking work for LoginScreen.
        if let Some(top) = screen_stack.last_mut() {
            top.after_present();
        }

        // Drain any worker responses produced since the last frame and
        // hand them to the TOP screen. For `Image` responses, decode
        // and insert into the cache before forwarding so screens can
        // clear inflight tracking after the cache is already populated.
        if let Some(w) = worker.as_ref() {
            while let Some(resp) = w.try_recv() {
                let resp = if let WorkResponse::Image {
                    url,
                    bytes: Ok(b),
                } = &resp
                {
                    if !url.starts_with("http") {
                        // Local file read (picker thumbnail / compose
                        // preview): forward the raw bytes untouched. The
                        // screen downscales via Texture::decode_scaled so
                        // a multi-megapixel decode never lands in the
                        // shared avatar/embed cache.
                        resp
                    } else {
                    // Avatars are alpha-masked into circles, so they must
                    // be 4 bpp RGBA. vita2d decodes JPEG to 3 bpp RGB
                    // (no alpha channel); under the mask that corrupts the
                    // pixels (the vertical black/red stripe bug). insert_rgba
                    // forces a 4 bpp copy; other images stay 3 bpp via insert.
                    let is_avatar = url.contains("/avatar_thumbnail/");
                    let inserted = if is_avatar {
                        texture_cache.insert_rgba(url.clone(), b)
                    } else {
                        texture_cache.insert(url.clone(), b)
                    };
                    match inserted {
                        Ok(()) => {
                            // Circular alpha mask so avatars composite
                            // cleanly over arbitrary backgrounds (banners).
                            if is_avatar {
                                if let Some(tex) = texture_cache.get(url) {
                                    tex.apply_circular_mask();
                                }
                            }
                            resp
                        }
                        Err(e) => {
                            bsky_log::log!("decode failed for {url}: {e}");
                            WorkResponse::Image {
                                url: url.clone(),
                                bytes: Err(format!("decode: {e}")),
                            }
                        }
                    }
                    }
                } else {
                    resp
                };
                if let Some(top) = screen_stack.last_mut() {
                    top.handle_worker_response(resp);
                }
            }
        }

        match action {
            ScreenAction::None => {}
            ScreenAction::Push(next) => {
                screen_stack.push(next);
            }
            ScreenAction::Pop => {
                if screen_stack.len() > 1 {
                    screen_stack.pop();
                }
            }
            ScreenAction::SwitchTab(target) => {
                handle_switch_tab(target, &mut screen_stack, auth_client.as_ref());
            }
            ScreenAction::AuthComplete { client, next } => {
                worker = Some(Worker::spawn(Arc::clone(&client)));
                auth_client = Some(client);
                screen_stack.clear();
                screen_stack.push(next);
            }
            ScreenAction::Logout => {
                // Drop the worker (closes its channel → the thread exits and
                // its AuthClient clone, holding the tokens, is freed) and our
                // own client handle BEFORE deleting the session files, so a
                // worker mid-refresh can't re-persist them after deletion.
                worker = None;
                auth_client = None;
                // Clear both auth-path session files so the post-logout login
                // form sees no resumable session regardless of which path
                // produced this session.
                let _ = std::fs::remove_file(bsky_auth::SESSION_PATH);
                let _ = std::fs::remove_file(bsky_oauth::OAUTH_SESSION_PATH);
                screen_stack.clear();
                screen_stack.push(Box::new(LoginScreen::idle()));
            }
        }
    }
}

/// Tab-bar tap handler: walk the stack from the bottom up looking for
/// a screen whose `top_level()` matches `target`. If found, truncate
/// the stack to (and including) that screen — preserves its in-memory
/// state. If not found, construct a fresh top-level instance and push
/// it as the new root (replacing the existing stack since top-levels
/// are mutually exclusive at the root level).
fn handle_switch_tab(
    target: TopLevel,
    stack: &mut Vec<Box<dyn Screen>>,
    auth: Option<&Arc<AuthClient>>,
) {
    if let Some(idx) = stack.iter().position(|s| s.top_level() == Some(target)) {
        stack.truncate(idx + 1);
        return;
    }
    let Some(client) = auth else {
        eprintln!("SwitchTab({target:?}) before auth — ignoring");
        return;
    };
    let next = make_top_level(target, Arc::clone(client));
    stack.clear();
    stack.push(next);
}

/// Construct a fresh instance of a top-level screen for `target`.
fn make_top_level(target: TopLevel, client: Arc<AuthClient>) -> Box<dyn Screen> {
    match target {
        TopLevel::Home => Box::new(TimelineScreen::new(client)),
        TopLevel::Profile => Box::new(ProfileScreen::new(client, None)),
        TopLevel::Notifications => Box::new(NotificationsScreen::new(client)),
        TopLevel::Search => Box::new(SearchScreen::new(client)),
        TopLevel::Messages => Box::new(ConversationListScreen::new(client)),
    }
}

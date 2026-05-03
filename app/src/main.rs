//! bsky-vita: PS Vita homebrew Bluesky client.
//!
//! Phase 3.1: worker-thread pattern for non-blocking PDS calls.
//!
//! - `LoginScreen` (initial) checks for `session.json` on its first
//!   frame; resumes if possible, otherwise shows the login form. Tap
//!   Login → "Authenticating…" → emits `ScreenAction::AuthComplete`,
//!   carrying the `Arc<AuthClient>` we use to spawn the worker thread.
//! - `ProfileScreen` (post-auth) dispatches `WorkRequest::GetOwnProfile`
//!   to the worker on its first frame; renders display name + handle +
//!   counts when the response arrives.
//!
//! Network calls now happen on a background thread; the render loop
//! drains responses via `worker.try_recv()` each frame and never blocks.
//! `Screen::after_present` survives for the pre-worker LoginScreen path
//! (`try_resume_existing_session`, `login_with_password`) — those run
//! *before* the worker exists.

use bsky_input::{Pad, Touch};
use bsky_render::{EmojiAtlas, Render};
use bsky_ui::{LoginScreen, Screen, ScreenAction, UiCtx};
use bsky_worker::Worker;

fn main() {
    let mut render = match Render::init() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vita2d_init failed: {e}; sleeping forever");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    };
    render.set_clear_color(bsky_render::theme::BACKGROUND);
    // Phase 3.3: try Inter (TTF, FreeType) first; fall back to PGF if the
    // bundled asset is missing or vita2d's font loader rejects it. PGF
    // rendering still works and produces the pre-3.3 visual; the user
    // sees a log line explaining the fallback.
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

    // Phase 3.4: optional Twemoji color-emoji atlas. If the asset is
    // missing on the device, emoji codepoints render as Inter fallback
    // (tofu) — app boots fine. Run `make push-emoji` to upload the
    // ~2.5 MB atlas separately from `make run`'s eboot.bin push.
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

    let mut pad = Pad::init();
    let touch = Touch::init();
    let mut ime = bsky_ime::Ime::new();

    // Always start with LoginScreen — its CheckingSession initial state
    // handles resume-from-session.json on the first frame.
    let mut screen: Box<dyn Screen> = Box::new(LoginScreen::new());

    // Worker is spawned the first time a screen returns `AuthComplete`
    // (LoginScreen → ProfileScreen). Pre-auth screens get `worker: None`
    // in their `UiCtx`.
    let mut worker: Option<Worker> = None;

    loop {
        let pf = pad.poll();
        let tf = touch.poll();
        let ctx = UiCtx {
            touches: &tf.points,
            pad: &pf,
            worker: worker.as_ref(),
            emoji: emoji_atlas.as_ref(),
        };

        // Render + collect transition action. The Frame's Drop happens
        // when this block ends, which presents the buffer.
        let action = {
            let mut frame = render.begin_frame();
            let action = screen.frame(&mut frame, &font, &ctx, &mut ime);
            if ime.is_active() {
                frame.pump_ime();
            }
            action
        };

        // Frame is now on the display; safe to do blocking work for
        // pre-worker screens (LoginScreen).
        screen.after_present();

        // Drain any worker responses produced since the last frame and
        // hand them to the active screen. Typically 0–1 per frame, but
        // we loop in case multiple complete simultaneously.
        if let Some(w) = worker.as_ref() {
            while let Some(resp) = w.try_recv() {
                screen.handle_worker_response(resp);
            }
        }

        match action {
            ScreenAction::None => {}
            ScreenAction::Goto(next) => {
                screen = next;
            }
            ScreenAction::AuthComplete { client, next } => {
                // First (and currently only) auth transition: spawn the
                // worker now that we have an AuthClient. Subsequent
                // re-auths would replace the worker; not needed in 3.1.
                worker = Some(Worker::spawn(client));
                screen = next;
            }
        }
    }
}

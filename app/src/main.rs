//! bsky-vita: PS Vita homebrew Bluesky client.
//!
//! Phase 2.5: screen-routing main loop.
//!
//! - `LoginScreen` (initial) checks for `session.json` on its first
//!   frame; resumes if possible, otherwise shows the login form. Tap
//!   Login → "Authenticating…" → transitions to `ProfileScreen` on
//!   success.
//! - `ProfileScreen` calls `getProfile` on the user's own DID; renders
//!   display name, handle, follower/following counts, did, pds.
//!
//! Network calls block in `Screen::after_present` so the just-rendered
//! "Loading…" / "Authenticating…" frame is on screen before we freeze.
//! Phase 3+ will refactor to a worker thread once timeline polling makes
//! per-call freezes intolerable.

use bsky_input::{Pad, Touch};
use bsky_render::Render;
use bsky_ui::{LoginScreen, Screen, ScreenAction, UiCtx};

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
    let font = render
        .load_default_pgf()
        .expect("load default PGF font");

    let mut pad = Pad::init();
    let touch = Touch::init();
    let mut ime = bsky_ime::Ime::new();

    // Always start with LoginScreen — its CheckingSession initial state
    // handles resume-from-session.json on the first frame.
    let mut screen: Box<dyn Screen> = Box::new(LoginScreen::new());

    loop {
        let pf = pad.poll();
        let tf = touch.poll();
        let ctx = UiCtx {
            touches: &tf.points,
            pad: &pf,
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

        // Frame is now on the display; safe to do blocking work.
        screen.after_present();

        if let ScreenAction::Goto(next) = action {
            screen = next;
        }
    }
}

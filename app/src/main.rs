//! bsky-vita: PS Vita homebrew Bluesky client.
//!
//! Phase 0.5: rustls spike. Performs a single HTTPS GET against the
//! Bluesky AppView's `describeServer` endpoint to validate that the
//! ureq + rustls + webpki-roots stack negotiates TLS 1.2/1.3 against
//! Cloudflare-fronted Bluesky infrastructure from the Vita target.
//!
//! Output goes to two places:
//!   - println!()  — visible if PrincessLog / psp2shell is installed.
//!   - ux0:data/BSKY00001/spike.log — pull via vitacompanion FTP on
//!     port 1337 (e.g., `curl ftp://$VITA_IP:1337/ux0:data/BSKY00001/spike.log`).

use std::fmt::Write as _;

const URL: &str = "https://api.bsky.app/xrpc/com.atproto.server.describeServer";
const LOG_DIR: &str = "ux0:data/BSKY00001";
const LOG_PATH: &str = "ux0:data/BSKY00001/spike.log";

fn main() {
    let report = run_spike();
    println!("{report}");

    // Best-effort log to disk. Never panic on IO errors so the LiveArea
    // bubble stays alive long enough for an FTP pull regardless of state.
    let _ = std::fs::create_dir_all(LOG_DIR);
    let _ = std::fs::write(LOG_PATH, &report);

    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn run_spike() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== bsky-vita: rustls spike ===");
    let _ = writeln!(out, "GET {URL}");

    let result = ureq::get(URL)
        .timeout(std::time::Duration::from_secs(30))
        .call();

    match result {
        Ok(resp) => {
            let _ = writeln!(out, "status: {}", resp.status());
            match resp.into_string() {
                Ok(body) => {
                    let head = &body[..body.len().min(500)];
                    let _ = writeln!(out, "body[..500]: {head}");
                }
                Err(e) => {
                    let _ = writeln!(out, "body read err: {e}");
                }
            }
        }
        Err(e) => {
            let _ = writeln!(out, "request err: {e}");
        }
    }
    let _ = writeln!(out, "=== spike done ===");
    out
}

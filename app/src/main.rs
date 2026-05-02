//! bsky-vita: PS Vita homebrew Bluesky client.
//!
//! Phase 0.5: rustls spike. Performs a single HTTPS GET against the
//! Bluesky AppView's `describeServer` endpoint to validate that the
//! ureq + rustls + webpki-roots stack negotiates TLS 1.2/1.3 against
//! Cloudflare-fronted Bluesky infrastructure from the Vita target.
//!
//! Output is visible via `cargo vita logs` (set $VITA_IP, then run on
//! hardware after `make run`). The app then sleeps so the LiveArea
//! bubble stays alive.

const URL: &str =
    "https://api.bsky.app/xrpc/com.atproto.server.describeServer";

fn main() {
    println!("=== bsky-vita: rustls spike ===");
    println!("GET {URL}");

    let result = ureq::get(URL)
        .timeout(std::time::Duration::from_secs(30))
        .call();

    match result {
        Ok(resp) => {
            let status = resp.status();
            println!("status: {status}");
            match resp.into_string() {
                Ok(body) => {
                    let head = &body[..body.len().min(300)];
                    println!("body[..300]: {head}");
                }
                Err(e) => println!("body read err: {e}"),
            }
        }
        Err(e) => println!("request err: {e}"),
    }

    println!("=== spike done; sleeping ===");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

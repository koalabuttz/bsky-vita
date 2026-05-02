//! bsky-vita: PS Vita homebrew Bluesky client.
//!
//! Phase 0: prove the toolchain. No FFI, no rendering, no networking.
//! `cargo vita build vpk` should produce a runnable VPK; running it on
//! hardware should print the line below to the debug UART (visible via
//! `cargo vita logs` once VITA_IP is set).

fn main() {
    println!("bsky-vita: phase 0 skeleton — VPK loaded successfully");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

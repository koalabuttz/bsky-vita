//! Re-exports of atrium types used across our crates.
//!
//! Phase 1 keeps this thin — just the handful of types `app/main.rs` touches.
//! Phase 2+ should expand it as our domain layer grows, so the rest of our
//! code only sees `bsky_models::*` and not `atrium_api::*` paths.

pub use atrium_api::agent::atp_agent::AtpSession;
pub use atrium_api::types::string::{AtIdentifier, Did, Handle};

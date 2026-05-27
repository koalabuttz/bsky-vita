//! Media plumbing for image posting: filesystem listing (for the file
//! picker), camera capture, and in-memory JPEG encoding.
//!
//! Each submodule isolates one chunk of Vita-only `sce*` FFI behind a
//! safe Rust surface, with a host fallback so the rest of the workspace
//! can `cargo check`/`cargo test` without the SDK present. This mirrors
//! the `bsky-video` crate's structure.
//!
//! Phase 7 build order: `fs` first (this commit), then `camera` + `jpeg`.

pub mod camera;
pub mod fs;
pub mod jpeg;

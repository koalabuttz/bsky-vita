//! Phase 0 placeholder. Will wrap libvita2d via vitasdk-sys and provide
//! a Rust-friendly Frame/Texture/Font surface plus a glyph atlas.
//!
//! Vita-only: all FFI lives behind `#[cfg(target_os = "vita")]`. On the
//! host, this crate is intentionally empty so workspace tests build.

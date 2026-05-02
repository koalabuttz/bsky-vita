# bsky-vita

Bluesky (AT Protocol) homebrew client for the PlayStation Vita, written in Rust.

**Status:** pre-alpha. Skeleton only — does not yet do anything useful.

## Build

Prerequisites:
- VitaSDK installed; `VITASDK` env var set (e.g. `/home/you/vitasdk`).
- Rust nightly via rustup; the toolchain channel is pinned in `rust-toolchain.toml`.
- `cargo-vita`: `cargo +nightly install cargo-vita`.

```sh
make build           # release VPK via cargo-vita
make run             # build + push to Vita over vitacompanion (set VITA_IP)
make test            # host-side library tests (skips Vita-only crates)
```

## Layout

```
app/                     # bin crate, packed into VPK; carries [package.metadata.vita]
crates/
  bsky-models/           # AT Protocol type re-exports + extensions    (host-testable)
  bsky-net/              # XRPC client; ureq + rustls + atrium glue    (host-testable)
  bsky-auth/             # session, JWT decode, refresh                (host-testable)
  bsky-store/            # flat-file LRU cache for posts / blobs       (host-testable)
  bsky-render/           # vita2d FFI wrapper; Vita-only
  bsky-ime/              # sceImeDialog wrapper; Vita-only
  bsky-input/            # sceCtrl + sceTouch wrapper; Vita-only
  bsky-ui/               # widgets + screens; Vita-only
```

## License

GPL-3.0-or-later. See `LICENSE`.

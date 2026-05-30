# bsky-vita

A native **Bluesky** (AT Protocol) client for the **PlayStation Vita**, written
in Rust. Browse your timeline and custom feeds, post threads with images and alt
text, watch video in colour, send DMs, and sign in with OAuth.

## Screenshots

<!-- Drop screenshots into docs/ and they'll render here. -->
| Timeline | Compose |
| --- | --- |
| ![Timeline](docs/timeline.png) | ![Compose](docs/compose.png) |

## Features

- **Read** — home timeline, custom/pinned feeds, full thread view, profiles with
  posts/replies/media/likes/feeds/lists/starter-packs tabs, notifications, and
  search (people + posts).
- **Post** — compose single posts or multi-post threads, replies, and quotes;
  attach up to 4 images (from the gallery or the Vita camera) with alt text.
- **Interact** — like, repost, follow/unfollow, and **delete your own posts**.
  Feed rows show "Reposted by …" / "Reply to …" context like the official apps.
- **Media** — inline images with a full-screen viewer, link cards, quote posts,
  and **colour video playback** with audio (hardware H.264 via `sceAvPlayer` +
  a bundled GPU YUV→RGB shader — no extra modules needed).
- **DMs** — read and send direct messages (`chat.bsky.convo.*`).
- **Sign in** — OAuth (PAR + PKCE + DPoP, via a small callback broker) or a
  Bluesky **app password**.
- **Quality of life** — circular avatars + colour emoji, on-screen control hints
  (press **SELECT**), and an on-screen keyboard for text entry.

## Requirements

- A **homebrew-enabled Vita** (HENkaku / h-encore / Enso) with **VitaShell**.

## Install

1. Download `bsky-vita.vpk` from the [latest release](../../releases/latest).
2. Copy it to your Vita and install it with **VitaShell**.
3. Launch **BskyVita** from the LiveArea.

Everything the app needs is baked into the VPK — no manual file copying.

### Signing in

- **OAuth (recommended):** enter your handle, then scan the on-screen QR code with
  your phone and approve. (DMs and all features are included.)
- **App password:** tap *"Use an app password instead"* and enter your handle +
  an [app password](https://bsky.app/settings/app-passwords). To use DMs with an
  app password, enable its *"Allow access to your direct messages"* option.

## Build from source

Prerequisites:
- [VitaSDK](https://vitasdk.org/) installed, with `VITASDK` set (e.g.
  `export VITASDK=/usr/local/vitasdk`).
- Rust **nightly** via rustup (the exact channel is pinned in
  `rust-toolchain.toml`).
- `cargo-vita`: `cargo install cargo-vita`.

```sh
make build    # release VPK via cargo-vita  -> target/.../release/bsky-vita.vpk
make run      # build + push eboot.bin to a networked Vita (set VITA_IP)
make install  # build + upload the full VPK to ux0:/download/ (set VITA_IP)
make test     # host-side library tests (Vita-only crates are skipped)
```

## Project layout

```
app/            Binary crate; assets in static/ are packed into the VPK.
broker/         Cloudflare Worker relaying the OAuth callback (self-hostable).
static-site/    OAuth client_metadata.json + callback pages.
crates/
  bsky-net/     XRPC/HTTP client: ureq + rustls + bundled roots; atrium HttpClient.
  bsky-auth/    Identity resolve, session store, app-password + OAuth agent dispatch.
  bsky-oauth/   AT Protocol OAuth (PAR+PKCE+DPoP) + file-backed session store.
  bsky-models/  Re-exports of the atrium AT Protocol types we use.
  bsky-render/  vita2d FFI: PGF/Inter/Twemoji text, textures, theme, QR, video shader.
  bsky-input/   sceCtrl + sceTouch input.
  bsky-ime/     sceImeDialog on-screen keyboard.
  bsky-ui/      Widgets, the Screen trait, and every screen.
  bsky-worker/  Background worker thread; typed WorkRequest/WorkResponse.
  bsky-video/   sceAvPlayer wrapper + GXM YUV->RGB colour shader.
  bsky-media/   Image decode (libpng/turbojpeg), camera, gallery, JPEG encode.
  bsky-log/     Disk-backed runtime log.
  bsky-store/   Reserved placeholder (caching lives in bsky-render/bsky-worker).
```

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE).

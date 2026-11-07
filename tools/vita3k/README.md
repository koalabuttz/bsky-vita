# Vita3K regression tests

These are black-box tests of a compiled Vita app. Vita3K executes the ARM
binary, real screen code processes mouse/controller input, and assertions inspect
screenshots with OCR and pixel checks. A loopback HTTP server serves typed XRPC
responses and JPEGs and records request times, cursors, and failures.

Each test owns a virtual X11 display, session bus, emulator configuration, cache,
and Vita filesystem. The `os0`, `sa0`, and `vs0` firmware mounts are copied into
each individual test case, including registry files that change during boot.
App installs, sessions, and writes go into the test directory. Your normal
emulator installation and Bluesky account are not used or modified.
Firmware is never downloaded or bundled by the harness.

## Run

On Linux, install Vita3K and its firmware/fonts first. The runner also needs
Python 3.10+, Pillow, Xvfb, xdotool, xwininfo, dbus-run-session, Tesseract with
English data, FFmpeg with libx264/AAC, and a Mesa software renderer. On Debian:

```sh
sudo apt install python3-pil xvfb xdotool x11-utils dbus-daemon \
  tesseract-ocr tesseract-ocr-eng ffmpeg mesa-vulkan-drivers
```

With the project's pinned Rust nightly, cargo-vita, and VitaSDK on `PATH`:

```sh
export VITASDK="$HOME/.local/opt/vitasdk" # or your existing SDK location
export PATH="$VITASDK/bin:$HOME/.local/bin:$PATH"
make test-emulator
```

The `emulator-tests` Cargo feature adds a bootstrap that reads
`ux0:/data/BSKY00001/emulator-test.json`. It accepts only an HTTP `127.0.0.1:port`
endpoint and creates a synthetic session through the real HTTP client. The
normal UI, worker, image decoder, GPU renderer, and retry machinery run unchanged
after that bootstrap. A missing configuration starts the ordinary login form.
The bootstrap is **absent from normal builds**. `make test-emulator` uses a
separate Cargo target directory so its VPK cannot replace a release package.

Run selected tests or choose another firmware directory:

```sh
make test-emulator VITA3K_TEST_ARGS='--test refresh --test video'
python3 tools/vita3k/run.py --vpk /absolute/path/to/test.vpk \
  --vita3k /absolute/path/to/vita3k --firmware /absolute/path/to/vita_fs
```

`--test` matches test-name substrings and can be repeated. `--output` selects a
new directory; existing directories are rejected. The default firmware directory
is `~/.local/share/Vita3K/Vita3K`. The default backend is software OpenGL using
Mesa llvmpipe. `--backend Vulkan` selects lavapipe instead. The packaged AppImage
needs a restricted host-Mesa library path; the runner constructs it inside the
test's temporary directory, preserving the AppImage's newer Wayland libraries.
`--hardware` disables software-driver selection, but your X server must then
support presentation on that GPU (ordinary Xvfb generally does not).

## What is asserted

- Login validation, password masking/reveal, and preservation when reopening IME.
- Actual 503 retry timing, the retry limit, permanent-error stopping, and manual retry.
- Refresh preserving existing posts until success, then replacing them.
- Pagination retrying the same cursor and appending the recovered page.
- Feed replies reaching a covered screen; stale A → B → A replies being ignored.
- JPEG URL normalization for two-image grids, shared request deduplication,
  invalid fullsize decode stopping, and manual viewer retry.
- More than 64 distinct images during rapid scrolling, followed by refresh, to
  exercise cache eviction and catch native emulator/guest crashes.
- Color emoji upload and rendering.
- Tab navigation and returning from compose.
- A generated local H.264/AAC clip: changing decoded color frames, pause holding
  pixels steady, resume/seek changing them again, and return to the feed.

The video fixture is seeded into the app's normal video cache. This tests the
decoder/rendering/controls path, not remote video resolution or downloading.
Audio samples are generated, but audible correctness is not asserted: the
headless run uses SDL's dummy audio device.

The login test uses the Qt frontend for its keyboard. Other tests use the SDL
frontend. The SDL frontend in Vita3K build 4277 does not start desktop text
input, so it cannot be used to validate typed IME text. Every test uses synthetic
data; real OAuth approval/refresh, posting, DMs, camera/gallery integration,
network conditions on a Vita, and physical GPU/decoder behavior still need
separate integration/device checks. Passing this suite does not prove those.

## Artifacts and failures

The runner exits nonzero for any failed test. It writes `results.json` with the
emulator version, backend, VPK SHA-256, case results, durations, and tracebacks.
Each case also retains `latest.png`, checkpoint screenshots, OCR text/input
images, `emulator.log`, `xvfb.log`, `requests.json`, and its isolated Vita files.
Artifacts default to `target/vita3k-tests/<UTC timestamp>/` and are gitignored.
Processes and helper children are terminated at test cleanup.

Native crashes and unsupported emulator behavior are failures, never silently
converted to passes. Preserve the failing artifacts when diagnosing whether the
fault belongs to this app, Vita3K, or its graphics driver. Some emulator error
logs (e.g. a missing session on a fresh install, injected HTTP errors, or a GLSL
compiler warning logged at error level) are expected, so log severity alone is
not the pass criterion.

### Observed compatibility failure

Vita3K `v0.2.1 4277-2f68c1bf` has reported a native SIGSEGV during the image-cache
scrolling stress case on this machine. The failure has reproduced across
OpenGL and Vulkan and also with conservative CPU/rendering settings. Its cause
has not been established; this is not evidence that the corresponding Vita
hardware path is safe or broken. The failure is intermittent. The stress case
stays enabled and treats any recurrence as a failure.

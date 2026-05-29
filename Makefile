APP_DIR := app
TITLE_ID := BSKY00001
VPK := target/armv7-sony-vita-newlibeabihf/release/bsky-vita.vpk

.PHONY: build build-fast run run-fast install push-creds push-font push-emoji push-mask push-gxp fetch-log fetch-gxp test clean help

help:
	@echo "Targets:"
	@echo "  build        Release VPK via cargo-vita (opt-level=z + LTO; ~90 s rebuild)"
	@echo "  build-fast   Dev VPK (opt-level=1, no LTO; ~20 s rebuild). Runtime is slower"
	@echo "               but builds iterate ~5x faster — use for layout/network bring-up,"
	@echo "               switch back to release for perf-sensitive testing."
	@echo "  install      First-time only: upload VPK to ux0:/download/ — install via VitaShell"
	@echo "  run          Release rebuild + push eboot.bin + launch (app must be installed)"
	@echo "  run-fast     Same as run, but with dev profile (~5x faster build)"
	@echo "  push-creds   Upload local credentials.toml to ux0:/data/$(TITLE_ID)/credentials.toml"
	@echo "  push-font    Upload app/static/Inter-Regular.ttf to ux0:/app/$(TITLE_ID)/ (one-shot)"
	@echo "  push-emoji   Upload app/static/twemoji.png to ux0:/app/$(TITLE_ID)/ (one-shot, ~2.5 MB)"
	@echo "  push-mask    Upload app/static/avatar_mask_96.png to ux0:/app/$(TITLE_ID)/ (one-shot, ~500 B)"
	@echo "  push-gxp     Upload app/static/video_yuv_{v,f}.gxp to ux0:/app/$(TITLE_ID)/ (one-shot)"
	@echo "  fetch-log    Pull ux0:data/$(TITLE_ID)/run.log via vitacompanion FTP"
	@echo "  fetch-gxp    Pull captured video_yuv_{v,f}.gxp from the device into app/static/"
	@echo "  test         Host-side library tests (excludes the Vita-target bin)"
	@echo "  clean        Remove build artifacts"
	@echo
	@echo "Required env: VITA_IP=<vita.lan.ip> for install/run*/push-*/fetch-log"
	@echo
	@echo "Big-asset workflow: 'make run' only updates eboot.bin (~5 MB). New or"
	@echo "changed assets in app/static/ require an explicit push (push-font /"
	@echo "push-emoji), or a full reinstall via 'make install'."

build:
	cd $(APP_DIR) && cargo vita build vpk -- --release

# Dev-profile build: opt-level=1 (per workspace [profile.dev]), no LTO,
# multiple codegen units. ~5x faster rebuild than release. Runtime is
# slower but acceptable for layout / network iteration.
build-fast:
	cd $(APP_DIR) && cargo vita build vpk

# First-time install: VPK lands at ux0:/download/, then install via VitaShell.
# `--ftp-create-dirs` lets curl mkdir if the download dir doesn't exist yet.
install: build
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	curl -sS --connect-timeout 5 --max-time 60 --ftp-create-dirs \
		-T $(VPK) "ftp://$$VITA_IP:1337/ux0:/download/bsky-vita.vpk"
	@echo "VPK uploaded. Open VitaShell on the Vita, navigate to ux0:/download/,"
	@echo "highlight bsky-vita.vpk, press Triangle, choose Install."

# Fast hardware iteration once the app is installed: replace eboot.bin in place
# and launch via vitacompanion. cargo-vita reads VITA_IP from env.
run: build
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	cd $(APP_DIR) && cargo vita build vpk --update --run -- --release

# Dev-profile run: ~5x faster build cycle. Use for iteration when build
# time matters more than runtime. Switch back to `make run` for perf
# checks or before committing.
run-fast: build-fast
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	cd $(APP_DIR) && cargo vita build vpk --update --run

# Push local credentials.toml to the Vita. credentials.toml is gitignored;
# copy credentials.toml.example to credentials.toml and fill in real values
# (Bluesky app password from bsky.app/settings/app-passwords — never the
# main account password).
push-creds:
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	@if [ ! -f credentials.toml ]; then \
		echo "credentials.toml not found."; \
		echo "Copy credentials.toml.example to credentials.toml and fill in real values."; \
		exit 1; \
	fi
	curl -sS --connect-timeout 5 --max-time 30 --ftp-create-dirs \
		-T credentials.toml "ftp://$$VITA_IP:1337/ux0:/data/$(TITLE_ID)/credentials.toml"
	@echo "credentials pushed to ux0:/data/$(TITLE_ID)/credentials.toml"

fetch-log:
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	curl -sS --connect-timeout 5 --max-time 15 \
		"ftp://$$VITA_IP:1337/ux0:/data/$(TITLE_ID)/run.log"

# Push the Inter TTF (~680 KB). Required once before the app first reads
# `app0:Inter-Regular.ttf`; subsequent rebuilds via `make run` re-use the
# already-pushed file. Re-run if the file in app/static changes.
push-font:
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	@if [ ! -f app/static/Inter-Regular.ttf ]; then \
		echo "app/static/Inter-Regular.ttf not found."; \
		echo "Drop the static Inter TTF in there first (see render_decisions memory)."; \
		exit 1; \
	fi
	curl -sS --connect-timeout 5 --max-time 60 \
		-T app/static/Inter-Regular.ttf \
		"ftp://$$VITA_IP:1337/ux0:/app/$(TITLE_ID)/Inter-Regular.ttf"
	@echo "Inter-Regular.ttf pushed."

# Push the Twemoji color-emoji atlas (~2.5 MB). Required once before the
# app first reads `app0:twemoji.png`. Vitacompanion's FTP can be flaky on
# big files (550 'Could not allocate memory') — if it fails, try again or
# destroy the running app first to free memory.
push-emoji:
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	@if [ ! -f app/static/twemoji.png ]; then \
		echo "app/static/twemoji.png not found."; \
		echo "Run 'python tools/build-twemoji.py' to generate it."; \
		exit 1; \
	fi
	curl -sS --connect-timeout 5 --max-time 120 \
		-T app/static/twemoji.png \
		"ftp://$$VITA_IP:1337/ux0:/app/$(TITLE_ID)/twemoji.png"
	@echo "twemoji.png pushed."

# Push the circular avatar mask (~500 B). Composited on top of avatars
# to fake circular rendering since vita2d has no clipping primitive.
push-mask:
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	@if [ ! -f app/static/avatar_mask_96.png ]; then \
		echo "app/static/avatar_mask_96.png not found."; \
		echo "Run 'python tools/build-avatar-mask.py' to generate it."; \
		exit 1; \
	fi
	curl -sS --connect-timeout 5 --max-time 30 \
		-T app/static/avatar_mask_96.png \
		"ftp://$$VITA_IP:1337/ux0:/app/$(TITLE_ID)/avatar_mask_96.png"
	curl -sS --connect-timeout 5 --max-time 30 \
		-T app/static/avatar_mask_field_96.png \
		"ftp://$$VITA_IP:1337/ux0:/app/$(TITLE_ID)/avatar_mask_field_96.png"
	@echo "avatar masks pushed (BG + FIELD_BG)."

# Pull the precompiled video-shader GXP blobs the app captured to its data
# dir (after a successful runtime compile on a console with libshacccg.suprx)
# into app/static/ so they bake into the VPK. One-time: run the app once with
# the module present, open a video, then `make fetch-gxp`.
fetch-gxp:
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	curl -sS --connect-timeout 5 --max-time 30 \
		-o app/static/video_yuv_v.gxp \
		"ftp://$$VITA_IP:1337/ux0:/data/$(TITLE_ID)/video_yuv_v.gxp"
	curl -sS --connect-timeout 5 --max-time 30 \
		-o app/static/video_yuv_f.gxp \
		"ftp://$$VITA_IP:1337/ux0:/data/$(TITLE_ID)/video_yuv_f.gxp"
	@echo "Captured GXP pulled into app/static/:"
	@ls -l app/static/video_yuv_v.gxp app/static/video_yuv_f.gxp

# Push the bundled precompiled video shaders for fast iteration (mirrors
# push-font — `make run` only updates eboot.bin, not assets). On a fresh
# install they bake into the VPK automatically; this is dev-only.
push-gxp:
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	@if [ ! -f app/static/video_yuv_v.gxp ] || [ ! -f app/static/video_yuv_f.gxp ]; then \
		echo "app/static/video_yuv_{v,f}.gxp not found."; \
		echo "Capture first: run the app once with libshacccg.suprx present,"; \
		echo "open a video, then 'make fetch-gxp'."; \
		exit 1; \
	fi
	curl -sS --connect-timeout 5 --max-time 30 \
		-T app/static/video_yuv_v.gxp \
		"ftp://$$VITA_IP:1337/ux0:/app/$(TITLE_ID)/video_yuv_v.gxp"
	curl -sS --connect-timeout 5 --max-time 30 \
		-T app/static/video_yuv_f.gxp \
		"ftp://$$VITA_IP:1337/ux0:/app/$(TITLE_ID)/video_yuv_f.gxp"
	@echo "Precompiled video shaders pushed (app0:video_yuv_{v,f}.gxp)."

# Library crates have no Vita-specific config and default to the host triple.
# The bin crate (bsky-vita-app) is target-locked to Vita and excluded here.
test:
	cargo test --workspace --exclude bsky-vita-app

clean:
	cargo clean
	rm -f *.vpk *.velf *.self

APP_DIR := app
TITLE_ID := BSKY00001
VPK := target/armv7-sony-vita-newlibeabihf/release/bsky-vita.vpk

.PHONY: build run install push-creds fetch-log test clean help

help:
	@echo "Targets:"
	@echo "  build        Release VPK via cargo-vita"
	@echo "  install      First-time only: upload VPK to ux0:/download/ — install via VitaShell"
	@echo "  run          Fast iteration: rebuild + push eboot.bin + launch (app must be installed)"
	@echo "  push-creds   Upload local credentials.toml to ux0:/data/$(TITLE_ID)/credentials.toml"
	@echo "  fetch-log    Pull ux0:data/$(TITLE_ID)/spike.log via vitacompanion FTP"
	@echo "  test         Host-side library tests (excludes the Vita-target bin)"
	@echo "  clean        Remove build artifacts"
	@echo
	@echo "Required env: VITA_IP=<vita.lan.ip> for install/run/push-creds/fetch-log"

build:
	cd $(APP_DIR) && cargo vita build vpk -- --release

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
		"ftp://$$VITA_IP:1337/ux0:/data/$(TITLE_ID)/spike.log"

# Library crates have no Vita-specific config and default to the host triple.
# The bin crate (bsky-vita-app) is target-locked to Vita and excluded here.
test:
	cargo test --workspace --exclude bsky-vita-app

clean:
	cargo clean
	rm -f *.vpk *.velf *.self

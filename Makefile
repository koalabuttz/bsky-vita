APP_DIR := app

.PHONY: build run test clean help

help:
	@echo "Targets:"
	@echo "  build  Release VPK via cargo-vita"
	@echo "  run    Build + push to Vita over vitacompanion (requires VITA_IP env var)"
	@echo "  test   Host-side library tests (excludes the Vita-target bin)"
	@echo "  clean  Remove build artifacts"

build:
	cd $(APP_DIR) && cargo vita build vpk -- --release

run: build
	@if [ -z "$$VITA_IP" ]; then echo "VITA_IP env var not set"; exit 1; fi
	cd $(APP_DIR) && cargo vita run -- --release

# Library crates have no Vita-specific config and default to the host triple.
# The bin crate (bsky-vita-app) is target-locked to Vita and excluded here.
test:
	cargo test --workspace --exclude bsky-vita-app

clean:
	cargo clean
	rm -f *.vpk *.velf *.self

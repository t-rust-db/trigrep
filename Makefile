# trigrep — `tg`, serverless trigram-indexed grep

.DEFAULT_GOAL := help

.PHONY: help build release test test-lib test-cli lint fmt install version clean

help: ## Show this help
	@echo ""
	@awk 'BEGIN {FS = ":.*?## "} \
	  /^# === .* ===$$/  { sub(/^# === /, ""); sub(/ ===$$/, ""); printf "\n\033[33m%s\033[0m\n", $$0 } \
	  /^[a-zA-Z0-9_-]+:.*?## / { printf "  \033[36m%-24s\033[0m %s\n", $$1, $$2 }' \
	  $(MAKEFILE_LIST)
	@echo ""

# === Build ===

build: ## Debug build of the `tg` binary
	cargo build

release: ## Optimised build (target/release/tg)
	cargo build --release

install: ## cargo install into ~/.cargo/bin as `tg`
	cargo install --path . --locked

# === Test ===

test: ## Full test suite (unit + CLI + crash-recovery)
	cargo test

test-lib: ## Library unit tests only (fastest inner loop)
	cargo test --lib

test-cli: ## CLI and crash-recovery integration tests
	cargo test --test cli --test crash

# === Quality ===

lint: ## clippy (deny warnings) + rustfmt check
	cargo clippy --all-targets -- -D warnings
	cargo fmt --all -- --check

fmt: ## rustfmt in place
	cargo fmt --all

# === Meta ===

version: ## Print the crate version (Cargo.toml [package].version)
	@grep -m1 '^version' Cargo.toml | sed -E 's/version = "(.*)"/\1/'

clean: ## Remove build artefacts
	cargo clean

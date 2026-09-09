# trigrep — `tg`, serverless trigram-indexed grep

.DEFAULT_GOAL := help

.PHONY: help build release test test-lib test-cli lint fmt install version clean check-panic-allows check-deny check-mvl-limit mcdc-obligations test-mcdc coverage check-coverage ci

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

lint: ## clippy (deny warnings, production panic lints in Cargo.toml) + rustfmt check + panic-allow policy
	cargo clippy --all-targets -- -D warnings
	cargo fmt --all -- --check
	$(MAKE) check-panic-allows

check-panic-allows: ## Policy gate: no panic-lint allows in production src/ (tools/check_panic_allows.py, from db-core)
	@python3 tools/check_panic_allows.py

check-deny: ## Supply-chain policy: advisories, licenses, bans, sources (deny.toml)
	@command -v cargo-deny >/dev/null 2>&1 || { echo "cargo-deny not found — cargo install cargo-deny --locked"; exit 1; }
	cargo deny check

# Documented exemptions from the qualified subset (cargo-mvl-limit), each
# with its reason next to the code it names. Empty = everything qualifies.
# Coarser than db-core's single `dyn`-boundary exemption (trigrep#9,
# follow-up needed): `eprintln!`/`format_args!` in the CLI's error and
# usage output, `Box<dyn Error>` as this crate's error type (cache.rs,
# main.rs), `proptest!`/`prop_oneof!` in the property-test modules
# (codec.rs, search.rs), and the explicit `'a` on search.rs's `Query`
# (the borrowed root path/regex live no longer than one search). None
# of these are unreviewed -- they're one grep away in this comment --
# but exempting whole files means other decisions in them are unchecked
# too. Narrowing this (a non-macro stderr writer, an enum error type, a
# proptest feature gate excluded from the scan) is the honest next step.
MVL_LIMIT_EXCLUDE := src/cache.rs src/main.rs src/codec.rs src/search.rs

check-mvl-limit: ## Qualified-subset gate (cargo-mvl-limit) over src/
	@command -v cargo-mvl-limit >/dev/null 2>&1 || { \
		echo "cargo-mvl-limit not found — install with:"; \
		echo "  cargo install --git https://github.com/mvl-lang/mvl-rust rust-limit --bin cargo-mvl-limit --locked"; \
		exit 1; }
	@cargo mvl-limit $$(find src -name '*.rs' $(foreach e,$(MVL_LIMIT_EXCLUDE),-not -path '$(e)') | sort) \
		&& echo "check-mvl-limit: all files in the qualified subset"

# === Evidence ===

# All of src/ — no obligation is exempted by file selection.
MCDC_FILES := $(shell find src -name '*.rs' | sort)

mcdc-obligations: ## Regenerate the committed MC/DC obligations snapshot (tests/mcdc/obligations.json)
	@command -v cargo-mvl-mcdc >/dev/null 2>&1 || { \
		echo "cargo-mvl-mcdc not found — install with:"; \
		echo "  cargo install --git https://github.com/mvl-lang/mvl-rust rust-mcdc --bin cargo-mvl-mcdc"; \
		exit 1; }
	@mkdir -p tests/mcdc
	@cargo-mvl-mcdc scan -o tests/mcdc/obligations.json $(MCDC_FILES)
	@echo "wrote tests/mcdc/obligations.json — commit it alongside the source change that shifted line numbers"

test-mcdc: mcdc-obligations ## MC/DC dashboard; fails if any multi-leaf obligation is undischarged (VERBOSE=1 for detail)
	cargo-mvl-mcdc harvest --obligations=tests/mcdc/obligations.json --run-dir=. 2>/dev/null \
		| python3 tools/mcdc_report.py $(if $(filter 1,$(VERBOSE)),--verbose,)

COVERAGE_MIN := 85

coverage: ## Line coverage report (cargo-llvm-cov) over lib + tests
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov not found — cargo install cargo-llvm-cov --locked; rustup component add llvm-tools-preview"; exit 1; }
	cargo llvm-cov clean --workspace
	cargo llvm-cov --no-report --all-targets
	cargo llvm-cov report
	cargo llvm-cov report --json --output-path target/llvm-cov.json

check-coverage: coverage ## Gate: fail if line coverage is below $(COVERAGE_MIN)%
	@python3 -c "import json, sys; \
	  p = json.load(open('target/llvm-cov.json'))['data'][0]['totals']['lines']['percent']; \
	  print(f'Line coverage: {p:.2f}% (threshold: $(COVERAGE_MIN)%)'); \
	  sys.exit(0 if p >= $(COVERAGE_MIN) else 1)"

# === CI ===

ci: ## Every CI gate locally, same order as .github/workflows/ci.yml
	$(MAKE) lint
	$(MAKE) check-deny
	$(MAKE) check-mvl-limit
	$(MAKE) test
	@echo "all CI gates passed"

fmt: ## rustfmt in place
	cargo fmt --all

# === Meta ===

version: ## Print the crate version (Cargo.toml [package].version)
	@grep -m1 '^version' Cargo.toml | sed -E 's/version = "(.*)"/\1/'

clean: ## Remove build artefacts
	cargo clean

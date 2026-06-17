# lumux — developer & cross-build commands.
#
# This repo is developed on Linux but targets the Windows host (and runs on
# macOS/Linux too). Native dev commands need only a Rust toolchain. Cross-
# building *runnable* macOS / Windows binaries from Linux uses cargo-zigbuild
# (Zig as the cross-linker); run `make bootstrap-cross` once to install it.
#
# Run `make help` for a categorized list.

CARGO        ?= cargo
# Put a local zig (e.g. ~/.local/bin) on PATH so zigbuild can find it.
export PATH  := $(HOME)/.local/bin:$(PATH)

# Cross targets.
WIN_MSVC     := x86_64-pc-windows-msvc
WIN_GNU      := x86_64-pc-windows-gnu
MAC_UNIVERSAL := universal2-apple-darwin
MAC_ARM      := aarch64-apple-darwin
MAC_X86      := x86_64-apple-darwin

DIST         := dist

# `install` replaces the destination by unlinking it first, so staging over a
# binary that is currently running (e.g. a persistent lumux server) never hits
# ETXTBSY the way `cp` (in-place truncate) does. -m755 also sets the exec bit.
INSTALL      := install -m755

.DEFAULT_GOAL := help

# ---------------------------------------------------------------------------
# Help
# ---------------------------------------------------------------------------
.PHONY: help
help: ## Show this help
	@echo "lumux make targets:"
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "} {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'

# ---------------------------------------------------------------------------
# Everyday dev (native, Rust toolchain only)
# ---------------------------------------------------------------------------
.PHONY: build
build: ## Native debug build of the whole workspace
	$(CARGO) build --workspace

.PHONY: release
release: ## Native optimized build
	$(CARGO) build --release --workspace

.PHONY: test
test: ## Run the full test suite
	$(CARGO) test --workspace

.PHONY: fmt
fmt: ## Format all code
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Check formatting without modifying files
	$(CARGO) fmt --all -- --check

.PHONY: lint
lint: ## Clippy with warnings denied (native target)
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: check
check: ## Fast type-check of the workspace
	$(CARGO) check --workspace

.PHONY: run
run: ## Run the client (use ARGS=... e.g. make run ARGS="ls")
	$(CARGO) run -p lumux -- $(ARGS)

.PHONY: clean
clean: ## Remove build artifacts and dist/
	$(CARGO) clean
	rm -rf $(DIST)

# ---------------------------------------------------------------------------
# Packaging — generate Homebrew / Scoop / winget manifests on demand.
# Manifests are derived from the release version + the published GitHub Release
# SHA256s, so they are generated into dist/ (gitignored) rather than checked in,
# avoiding stale-hash drift. Copy each into its target repo by hand.
# ---------------------------------------------------------------------------
.PHONY: packaging
packaging: ## Generate dist manifests for a release (VERSION=x.y.z, default: workspace version)
	scripts/gen-packaging.sh $(VERSION)

# ---------------------------------------------------------------------------
# Release version bump. Delegates to scripts/bump-version.sh, which rewrites the
# workspace version + the four internal dependency pins in Cargo.toml, refreshes
# Cargo.lock via a build, verifies `lumux --version`, then commits and tags.
# Pushing the tag (which triggers release CI) is left to you.
# ---------------------------------------------------------------------------
.PHONY: bump
bump: ## Bump version, refresh lockfile, commit & tag (VERSION=x.y.z required)
	scripts/bump-version.sh $(VERSION)

# ---------------------------------------------------------------------------
# CI gate — what the pipeline runs. Includes the Windows cross-CHECK that
# guards the Windows backend from bit-rot when building only on Linux.
# ---------------------------------------------------------------------------
.PHONY: ci
ci: fmt-check lint test check-windows ## Run the full CI gate locally

.PHONY: check-windows
check-windows: ## Type-check the Windows (msvc) target — no linking needed
	rustup target add $(WIN_MSVC) >/dev/null 2>&1 || true
	$(CARGO) check --workspace --target $(WIN_MSVC)

.PHONY: check-macos
check-macos: ## Type-check both macOS targets
	rustup target add $(MAC_ARM) $(MAC_X86) >/dev/null 2>&1 || true
	$(CARGO) check --workspace --target $(MAC_ARM)
	$(CARGO) check --workspace --target $(MAC_X86)

# ---------------------------------------------------------------------------
# Cross-build runnable binaries from Linux (requires cargo-zigbuild + zig).
# ---------------------------------------------------------------------------
.PHONY: bootstrap-cross
bootstrap-cross: ## Install cargo-zigbuild (you must also have `zig` on PATH)
	$(CARGO) install cargo-zigbuild
	@command -v zig >/dev/null 2>&1 \
		&& echo "zig found: $$(zig version)" \
		|| echo "WARNING: 'zig' not on PATH. Install Zig 0.13+ and ensure it is on PATH (e.g. ~/.local/bin)."

.PHONY: dist-macos
dist-macos: ## Cross-build universal macOS binary -> dist/macos/
	rustup target add $(MAC_ARM) $(MAC_X86) >/dev/null 2>&1 || true
	$(CARGO) zigbuild --release --target $(MAC_UNIVERSAL) -p lumux
	@mkdir -p $(DIST)/macos
	$(INSTALL) target/$(MAC_UNIVERSAL)/release/lumux  $(DIST)/macos/lumux
	@echo "Built universal macOS binary in $(DIST)/macos/ (unsigned)."

.PHONY: dist-windows
dist-windows: ## Cross-build runnable Windows .exe (gnu) -> dist/windows/
	rustup target add $(WIN_GNU) >/dev/null 2>&1 || true
	$(CARGO) zigbuild --release --target $(WIN_GNU) -p lumux
	@mkdir -p $(DIST)/windows
	$(INSTALL) target/$(WIN_GNU)/release/lumux.exe  $(DIST)/windows/lumux.exe
	@echo "Built Windows binary in $(DIST)/windows/ (gnu/MinGW ABI)."
	@echo "NOTE: the production target is msvc; this gnu build is for quick"
	@echo "      cross-testing from Linux. Use Windows CI for msvc release artifacts."

.PHONY: dist-linux
dist-linux: ## Build optimized Linux binary -> dist/linux/
	$(CARGO) build --release -p lumux
	@mkdir -p $(DIST)/linux
	# install (not cp) unlinks the old inode first, so a running lumux server
	# from a previous build doesn't cause ETXTBSY ("Text file busy").
	$(INSTALL) target/release/lumux  $(DIST)/linux/lumux
	@echo "Built Linux binary in $(DIST)/linux/."

.PHONY: dist
dist: dist-linux dist-macos dist-windows ## Cross-build for all three platforms
	@echo "All platform binaries staged under $(DIST)/."

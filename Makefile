# wmux — developer & cross-build commands.
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
# binary that is currently running (e.g. a persistent wmuxd daemon) never hits
# ETXTBSY the way `cp` (in-place truncate) does. -m755 also sets the exec bit.
INSTALL      := install -m755

.DEFAULT_GOAL := help

# ---------------------------------------------------------------------------
# Help
# ---------------------------------------------------------------------------
.PHONY: help
help: ## Show this help
	@echo "wmux make targets:"
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
	$(CARGO) run -p wmux -- $(ARGS)

.PHONY: clean
clean: ## Remove build artifacts and dist/
	$(CARGO) clean
	rm -rf $(DIST)

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
dist-macos: ## Cross-build universal macOS binaries -> dist/macos/
	rustup target add $(MAC_ARM) $(MAC_X86) >/dev/null 2>&1 || true
	$(CARGO) zigbuild --release --target $(MAC_UNIVERSAL) -p wmux -p wmuxd
	@mkdir -p $(DIST)/macos
	$(INSTALL) target/$(MAC_UNIVERSAL)/release/wmux  $(DIST)/macos/wmux
	$(INSTALL) target/$(MAC_UNIVERSAL)/release/wmuxd $(DIST)/macos/wmuxd
	@echo "Built universal macOS binaries in $(DIST)/macos/ (unsigned)."

.PHONY: dist-windows
dist-windows: ## Cross-build runnable Windows .exe (gnu) -> dist/windows/
	rustup target add $(WIN_GNU) >/dev/null 2>&1 || true
	$(CARGO) zigbuild --release --target $(WIN_GNU) -p wmux -p wmuxd
	@mkdir -p $(DIST)/windows
	$(INSTALL) target/$(WIN_GNU)/release/wmux.exe  $(DIST)/windows/wmux.exe
	$(INSTALL) target/$(WIN_GNU)/release/wmuxd.exe $(DIST)/windows/wmuxd.exe
	@echo "Built Windows binaries in $(DIST)/windows/ (gnu/MinGW ABI)."
	@echo "NOTE: the production target is msvc; this gnu build is for quick"
	@echo "      cross-testing from Linux. Use Windows CI for msvc release artifacts."

.PHONY: dist-linux
dist-linux: ## Build optimized Linux binaries -> dist/linux/
	$(CARGO) build --release -p wmux -p wmuxd
	@mkdir -p $(DIST)/linux
	# install (not cp) unlinks the old inode first, so a running wmuxd daemon
	# from a previous build doesn't cause ETXTBSY ("Text file busy").
	$(INSTALL) target/release/wmux  $(DIST)/linux/wmux
	$(INSTALL) target/release/wmuxd $(DIST)/linux/wmuxd
	@echo "Built Linux binaries in $(DIST)/linux/."

.PHONY: dist
dist: dist-linux dist-macos dist-windows ## Cross-build for all three platforms
	@echo "All platform binaries staged under $(DIST)/."

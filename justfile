# Forge dev-workflow runner.
#
# Install just: `cargo install just` (or `brew install just`, `apt install just`).
# Run `just` with no args to see all recipes.

set shell := ["bash", "-ceuo", "pipefail"]

# Show available recipes.
default:
    @just --list

# -----------------------------------------------------------------------------
# Dev workflow
# -----------------------------------------------------------------------------

# Run the desktop app in dev mode. Spawns Vite at :5173 via Tauri's
# `beforeDevCommand`, then launches the shell webview against it.
dev:
    @command -v cargo-tauri >/dev/null || { echo >&2 "cargo-tauri not found. Install: cargo install tauri-cli --version '^2.0' --locked"; exit 1; }
    cd crates/forge-shell && cargo tauri dev

# Start only the Vite dev server (use with `just dev-shell` in another terminal).
dev-vite:
    cd web && pnpm --filter app dev

# Launch only the Tauri shell (Vite must already be running on :5173).
dev-shell:
    cargo run -p forge-shell

# Build everything: Rust workspace (debug) + full pnpm workspace.
build:
    cargo build --workspace
    cd web && pnpm install --frozen-lockfile && pnpm -r build

# Release build of the three shippable binaries. The Tauri shell still
# loads from web/packages/app/dist, so the pnpm build is required.
release-bins:
    cd web && pnpm install --frozen-lockfile && pnpm -r build
    cargo build --release -p forge-cli -p forge-session -p forge-shell

# Drives `cargo tauri build`, which runs `beforeBuildCommand` (production
# pnpm build) and then bundles `forge-shell`. Pass a comma-separated bundle
# list to narrow the targets — e.g. `just bundle rpm`, `just bundle deb`,
# `just bundle rpm,deb`. Defaults to `all`, which honours `tauri.conf.json`
# (.deb / .rpm / .AppImage on Linux; .dmg on macOS; .msi on Windows).
# Output lands under `target/release/bundle/<format>/`.
# Production bundle: release-mode Tauri installers for the host platform.
bundle bundles="all":
    @command -v cargo-tauri >/dev/null || { echo >&2 "cargo-tauri not found. Install: cargo install tauri-cli --version '^2.0' --locked"; exit 1; }
    cd web && pnpm install --frozen-lockfile
    cd crates/forge-shell && cargo tauri build --bundles {{bundles}}

# Auto-format Rust sources.
fmt:
    cargo fmt --all

# -----------------------------------------------------------------------------
# CI-mirrored checks — CI calls these recipes directly
# -----------------------------------------------------------------------------

# Rust lane: fmt --check, cargo check, clippy (warnings denied), rustdoc.
# Mirrors the Rust lint steps in .github/workflows/ci.yml `check` job.
check-rust:
    cargo fmt --all -- --check
    cargo check --all-targets
    cargo clippy --workspace --all-targets -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

# Web lane: typecheck + design-token drift gate + raw-button gate.
# Mirrors the pnpm lint steps in the `frontend` job. Assumes deps installed.
# `check-voice` is informational and lives in its own recipe; promote
# it into this lane once the corpus is clean (F-699 follow-up #820).
check-web:
    cd web && pnpm -r typecheck
    cd web && pnpm check-tokens
    cd web && pnpm check-raw-buttons

# Voice-terminology helper (F-699 follow-up #820, item 6). Informational:
# always exits 0, prints findings for human triage. Promote into
# `check-web` once the corpus is clean.
check-voice:
    cd web && pnpm check-voice

# Markdown link-rot gate (F-705). Offline lane only — mirrors the
# `internal-links` job in .github/workflows/link-check.yml. The
# nightly external sweep is CI-only because it needs network and
# scheduled cadence.
check-links:
    lychee --offline --no-progress 'docs/**/*.md' 'crates/**/README.md' 'web/packages/**/README.md' README.md CHANGELOG.md AGENTS.md

# Both lanes.
check: check-rust check-web

# Rust test suite.
test-rust:
    cargo test --all

# Serial `#[ignore]`-gated Rust tests that can't share a process-wide
# resource with parallel test binaries. Currently: the F-154 McpManager
# subprocess integration test, which conflicts with tokio's single
# process-wide SIGCHLD reaper when other test binaries are also spawning
# and reaping children in parallel. Run single-threaded, single-binary.
test-rust-serial:
    cargo test -p forge-mcp -- --ignored --test-threads=1

# Tauri webview integration tests. Gated on the `webview-test` feature
# because `tauri::test::mock_builder` pulls in the full Tauri runtime; keeping
# it off `test-rust`'s default build lets hosts without WebKitGTK headers
# still run the pure-Rust suite. Covers:
#   - forge-shell/tests/ipc_*.rs           (F-020 / F-051 / F-068 / F-069 / F-125)
#   - forge-shell/tests/approval_commands.rs (F-036)
test-rust-webview:
    cargo test -p forge-shell --features webview-test

# Web test suite.
test-web:
    cd web && pnpm -r test

# Both lanes.
test: test-rust test-rust-serial test-rust-webview test-web

# Regenerate the TypeScript bindings from Rust types. ts-rs emits each
# `#[ts(export)]` type to `web/packages/ipc/src/generated/` as a side effect
# of the auto-generated `export_bindings_*` test cases — `cargo build` does
# NOT trigger export, only `cargo test`. Run this after editing any Rust
# type that derives `TS`, then commit the regenerated files.
generate-ts:
    cargo test --workspace --quiet --tests export_bindings_

# Verify committed TS bindings match what ts-rs would regenerate from the
# current Rust sources. Self-contained: regenerates first, then diffs. CI
# wires this in as a drift gate — see .github/workflows/ci.yml.
ts-check: generate-ts
    git diff --exit-code web/packages/ipc/src/generated/

# Supply-chain audits. Local use only; CI uses dedicated actions for caching
# and for surfacing advisories as PR annotations. cargo-deny consults the
# same RustSec advisory DB as cargo-audit while also enforcing licenses,
# bans, and sources — see docs/dev/security.md.
# Requires: cargo install cargo-deny
audit:
    cargo deny check --all-features
    cd web && pnpm audit --audit-level moderate

# -----------------------------------------------------------------------------
# Phase 1 smoke
# -----------------------------------------------------------------------------

# Phase 1 smoke gate — build + CLI-only UATs (UAT-09, UAT-10, UAT-13).
# Fastest pre-Phase-2 confidence check; no browser required.
smoke:
    cargo build --workspace
    ./docs/testing/phase1-uat.sh --cli-only

# -----------------------------------------------------------------------------
# Cleanup
# -----------------------------------------------------------------------------

# Drop all build artifacts (Rust + web).
clean:
    cargo clean
    rm -rf web/packages/app/dist web/packages/*/node_modules web/node_modules

# Phase 2 UAT Setup

One-time setup needed before running `docs/testing/phase2-uat.sh` or the
Playwright specs in `web/packages/app/tests/phase2/`. The UAT plan itself is
`docs/testing/phase2-uat.md`.

## 1. Rust binaries

```bash
cargo build --workspace
# Or, for faster UAT runs:
cargo build --release -p forge-cli -p forge-session -p forge-shell -p forge-mcp
```

The harness looks in `target/release/` first, then `target/debug/`. Either
works. `forge-shell` is needed for the real-shell Playwright specs; the bash
UATs (UAT-04, UAT-10) need `forge` and the two MCP mock binaries.

## 2. MCP mock servers

UAT-04 and UAT-10 invoke `forge mcp import` against a stdio mock and an HTTP
mock. Both ship under `crates/forge-mcp/` as named binaries:

```bash
cargo build -p forge-mcp \
  --bin forge-mcp-mock-stdio \
  --bin forge-mcp-mock-http
```

The harness expects them at `target/{release,debug}/forge-mcp-mock-stdio` and
`target/{release,debug}/forge-mcp-mock-http`. The HTTP mock takes a `--port`
flag and listens on `127.0.0.1`; the harness picks an ephemeral port at run
time and tears the process down on EXIT.

## 3. pnpm workspace

```bash
cd web
pnpm install
pnpm --filter app build           # confirms the app still compiles
pnpm check-tokens                 # design-token drift gate (still load-bearing for Phase 2 — guards F-428 known gap)
```

`check-tokens` is defined on the root `web/package.json`, not on the `app`
workspace. Run it from `web/` (not from `web/packages/app/`).

If `pnpm` is missing: `corepack enable && corepack prepare pnpm@9.12.0 --activate`.

## 4. Playwright

```bash
cd web/packages/app
pnpm exec playwright install      # downloads Chromium; ~170MB
```

Install once per machine. The Playwright version pinned in `package.json`
(`@playwright/test ^1.48.0`) picks the browser build. System dependencies on
Linux:

```bash
sudo pnpm exec playwright install-deps   # one-time, distro-dependent
# or on Fedora specifically:
sudo dnf install -y alsa-lib nss atk cups-libs libdrm libxkbcommon \
  at-spi2-atk mesa-libgbm libXcomposite libXdamage libXrandr libXScrnSaver
```

Phase 2's Playwright config registers a `phase2` project that points at
`web/packages/app/tests/phase2/`. The harness invokes it via
`pnpm run test:e2e:phase2`.

## 5. Provider selection

Phase 2 UATs use `MockProvider` end-to-end. No remote provider is required.

`forged` picks its provider from, in precedence order:

1. `--provider <spec>` flag
2. `FORGE_PROVIDER` env var
3. `MockProvider` default (uses `FORGE_MOCK_SEQUENCE_FILE` if set)

For per-UAT scripted turns, write a JSON array of NDJSON-script strings to a
file and export `FORGE_MOCK_SEQUENCE_FILE`. The harness installs a default
single-turn script up front; UATs that need richer behavior (e.g. UAT-05's
sub-agent spawn) override the file before launching `forged`.

## 6. tauri-driver (for real-shell Playwright specs)

Required for the layout / editor / terminal / monitor specs (UAT-01, UAT-02,
UAT-03, UAT-05, UAT-06, UAT-07, UAT-08). The state-coverage spec (UAT-09)
runs against the Vite dev build with mocked IPC and does not need
`tauri-driver`.

```bash
cargo install tauri-driver --locked
# Linux also needs webkit2gtk-driver:
sudo dnf install -y webkit2gtk4.1-driver   # Fedora
```

## 7. Run the harness

```bash
# Full suite (build + run):
./docs/testing/phase2-uat.sh --build

# Just the bash UATs (UAT-04, UAT-10) — fastest, no browser:
./docs/testing/phase2-uat.sh --cli-only

# Just the Playwright GUI specs:
./docs/testing/phase2-uat.sh --gui-only

# Focus on one UAT:
./docs/testing/phase2-uat.sh --test UAT-04
./docs/testing/phase2-uat.sh --gui-only --test UAT-01

# Debug a Playwright failure interactively:
cd web/packages/app && pnpm run test:e2e:ui -- --grep phase2
```

## What to expect first run

- **CLI UATs** (UAT-04, UAT-10) should pass once the Rust build is green and
  the two MCP mock binaries exist. They're fully automated.
- **Playwright UATs**:
  - **Runnable with mocked IPC**: UAT-09 (state coverage).
  - **Runnable with `tauri-driver`**: UAT-01 (layout), UAT-02 (editor),
    UAT-03 (terminal), UAT-05 (agents/sub-agents), UAT-06 (monitor),
    UAT-07 (@-context picker), UAT-08 (re-run replace).
  - **Instrumentation gaps flagged in the plan**: drop-zone visual feedback
    (UAT-01), Monaco diagnostic-line introspection (UAT-02), xterm cell-level
    output (UAT-03), agent provenance metadata (UAT-05), promote-to-foreground
    (UAT-06), in-picker truncation notice (UAT-07), Branch button data-testid
    (UAT-08), TerminalPane error data-testid (UAT-09). Each is documented
    inline in `phase2-uat.md` so the spec author can either add the missing
    `data-testid` first or scope the test by class / accessible name with the
    documented caveat.

Spec files should carry `test.skip(...)` with a human-readable reason for any
UAT that depends on an unmerged instrumentation hook; the `README.md` next to
them should map every reason to its tracking issue.

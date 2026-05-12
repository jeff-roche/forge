# Phase 2 User Acceptance Test Plan

**Scope:** Phase 2: Full Layout + MCP — pane layout (splits H/V/grid, drag-to-dock), EditorPane (Monaco + LSP), TerminalPane (`forge-term` + xterm.js), Files sidebar, `forge-mcp` (stdio + http transports, `.mcp.json` schema, `forge mcp import`), `forge-agents` (`.agents/*.md`, `AGENTS.md` auto-injection, orchestrator, sub-agent banners), background agents, Agent Monitor, @-context picker, Re-run (Replace; Branch scaffolded).

**Outcome gate:** A user can launch Forge, lay out a four-pane workspace (FilesSidebar + EditorPane + TerminalPane + ChatPane), drag the layout into a 2×2 grid, see the layout persist across a restart, and use it to drive an end-to-end agent flow (open a file, run a terminal command, exchange messages, invoke an MCP tool, observe a sub-agent banner, monitor a background agent).

> **Authoring convention:** new UATs in this plan should follow the `contract-level` / `acceptance-only` labelling defined in [`docs/testing/uat-conventions.md`](./uat-conventions.md). The convention's migration table tentatively classifies each Phase 2 UAT below; back-labelling each scenario in place is tracked under F-327 follow-up. The `contract-level` set feeds the persistent smoke-UAT suite (F-326).

---

## Known gap before reading further

Phase 2 ships 151 closed issues, but several release-prep items shipped under the milestone are **not yet reflected on disk** and would block a release-readiness GO without remediation. None of these block the UATs themselves — but reviewers should understand that "all UATs pass" does not equal "shippable":

- `CHANGELOG.md` `[Unreleased]` is empty despite 30+ user-visible Phase 2 changes (release-blocker per F-446 docs audit).
- `docs/architecture/ipc-contracts.md` is missing 28+ Tauri commands and the Event enum disagrees with the Rust source (F-432, F-433).
- `docs/architecture/crate-architecture.md` §3.3 lags 8 Event variants behind code (F-433).
- `docs/build/approach.md` and `AGENTS.md` reference the nonexistent `web/apps/` directory (F-430, F-431).
- `web/packages/app/src/shell/StatusBar.css` uses 19 unregistered `--fg-*` design tokens (F-428).
- No ADRs exist for F-149 (sandbox cgroup) or F-155 (MCP consolidation) (F-436).

These are tracked in the F-446 docs-audit children (all closed) and the F-419 frontend-review children, and will be force-graded by `forge-milestone-release-readiness`.

---

## What this plan covers

Phase 2 ships 151 closed issues. Rather than restate each issue's Definition of Done as a UAT (those are already enforced by unit tests; collectively ~678 checkboxes), the cases below verify **user-observable behavior that depends on multiple tickets working together**. Backend primitives with no user surface — concurrency edge cases, atomic-config-file internals, frame-cap enforcement, ts-rs binding regeneration, the `http_roundtrip` flake fix, and per-IPC-command authz — are called out at the bottom and are covered by crate-level unit tests, not by UATs.

12 UATs across 9 GUI flows, 2 disk/state flows, 1 cross-cutting state-coverage sweep, and 1 hybrid (settings + approvals).

Automation vehicle:
- **GUI UATs** — Playwright driving the Tauri app via `tauri-driver`. Selectors prefer existing `data-testid` attributes already in the components; gaps are flagged inline per UAT and listed in the Instrumentation gaps appendix.
- **Disk / state UATs** — bash harness invoking `forge` / `forged` / `forge mcp import` and inspecting filesystem and IPC outcomes, in the same style as `docs/testing/phase1-uat.sh`.

---

## Prerequisites

| Item | Requirement |
|------|-------------|
| Rust build | `cargo build --workspace` succeeds |
| Binaries | `forge`, `forged`, `forge-shell` on `$PATH` or under `target/{debug,release}` |
| Web build | `pnpm install && pnpm --filter app build` from `web/` succeeds |
| Design tokens | `pnpm check-tokens` from `web/` passes (guards F-428 drift) |
| Playwright | `pnpm --filter app exec playwright install` has been run |
| Tauri driver | `tauri-driver` available on `$PATH` (`cargo install tauri-driver`) |
| Mock provider | `FORGE_MOCK_SEQUENCE_FILE` points to a NDJSON-script JSON array (Phase 1 harness format) |
| Mock MCP servers | A stdio mock (`forge-mcp-mock-stdio` from `crates/forge-mcp/tests/fixtures/`) and an http mock (`forge-mcp-mock-http`, listens on a loopback ephemeral port) are built (`cargo build -p forge-mcp --bin forge-mcp-mock-stdio --bin forge-mcp-mock-http`) |
| Mock agent | `.agents/test-agent.md` exists in the scratch workspace |
| AGENTS.md fixture | `AGENTS.md` (≤ 256 KiB per F-352) exists at scratch workspace root |
| Workspace | An empty temp directory per run (no pre-existing `.forge/` or `.mcp.json`) |
| Spec directory | `web/packages/app/tests/phase2/` exists; one `uat-NN-<slug>.spec.ts` per Playwright UAT |

**No external provider prerequisite.** Phase 2 UATs use MockProvider and mock MCP servers throughout — no remote endpoint required.

---

## UAT-01: Outcome gate — four-pane layout, drag-to-dock, persistence

**Scope:** F-117 (split-pane primitives) + F-118 (drag-to-dock + drop-zone detection) + F-119 (min-width collapse rules) + F-120 (layout persistence via `.forge/layouts.json`) + F-126 (activity bar + FilesSidebar + tree IPC) + F-150 (EditorPane integration into GridContainer).
**Vehicle:** Playwright + `tauri-driver`, MockProvider session.
**Why this test exists:** this is Phase 2's outcome statement restricted to what shipped end-to-end. If UAT-01 passes, the layout shell that all subsequent UATs depend on is wired correctly.

Preparation:
```bash
forge session new agent test-agent --workspace "$WS"
echo "hello from phase 2" > "$WS/readable.txt"
```

| Step | Action | Expected |
|------|--------|----------|
| 1 | Launch `forge-shell`, click the seeded session card | Session window opens; default layout shows `[data-testid="chat-pane"]` filling main area |
| 2 | Toggle the Files sidebar | `[data-testid="files-sidebar"]` mounts on the left; tree of `$WS` renders |
| 3 | Open `readable.txt` from the sidebar | `[data-testid="editor-pane"]` mounts in the main area; `[data-testid="editor-pane-iframe"]` becomes visible |
| 4 | Open a terminal pane (header action / shortcut) | `[data-testid="terminal-pane"]` mounts; `[data-testid="terminal-pane-host"]` renders |
| 5 | Drag the EditorPane title-bar onto the right-half drop zone of TerminalPane | Layout splits horizontally; both panes have a `[data-leaf-id]` attribute and a non-zero size |
| 6 | Drag the TerminalPane title-bar onto the bottom-half drop zone of EditorPane | Layout becomes a 2×2 grid (FilesSidebar + EditorPane | TerminalPane / ChatPane); each leaf is queryable via `[data-testid^="grid-leaf-"]` |
| 7 | Resize the splitter between EditorPane and TerminalPane | Both panes' rendered widths/heights update; no flicker; xterm.js `fitAddon.fit()` triggers a `terminal_resize` IPC call |
| 8 | Close the Session window | No console warnings; layoutStore persists the current `LayoutTree` shape (verify via `$XDG_DATA_HOME/forge/sessions/<id>/layout.json` or equivalent persistence path) |
| 9 | Re-open the same session card | Layout restores: same 2×2 grid, same splitter ratios, FilesSidebar still toggled on |

**Failure criteria:** drag-to-dock does not split, splitters do not persist their ratios, `terminal_resize` is not invoked on resize, or the layout reverts to default after restart.

**Instrumentation gap:** drop-zone visual feedback is not externally observable — there is no `data-testid="drop-zone-{zone}"` on `DropZoneOverlay` (`web/packages/app/src/layout/GridContainer.tsx:75-90`). Steps 5–6 verify the *outcome* of a successful drop; the in-flight visual state is out of UAT scope until that selector lands.

---

## UAT-02: EditorPane — Monaco-in-iframe + LSP diagnostics + go-to-definition

**Scope:** F-121 (`web/packages/monaco-host` iframe + `monaco-languageclient`) + F-122 (EditorPane component + `read_file`/`write_file`/`tree` IPC) + F-123 (`forge-lsp` server discovery + download bootstrap + stdio lifecycle) + F-148 (LSP-spawn architecture reconciliation).
**Vehicle:** Playwright + `tauri-driver`, real `forge-lsp` against a TypeScript fixture file.

Preparation:
```bash
mkdir -p "$WS/src"
cat > "$WS/src/example.ts" <<'TS'
function greet(name: string): string {
  return `hello ${name}`;
}
const unused: number = greet("forge");  // type error: string assigned to number
greet("world");
TS
```

| Step | Action | Expected |
|------|--------|----------|
| 1 | Open `src/example.ts` from FilesSidebar | `[data-testid="editor-pane-iframe"]` becomes visible after `[data-testid="editor-pane-file-loading"]` clears |
| 2 | Wait for LSP attach (≤ 3s) | Diagnostic underline appears on `unused` line in the iframe (Monaco renders a wavy underline; Playwright asserts via iframe DOM scrape) |
| 3 | Hover the diagnostic | Tooltip with the LSP message renders inside the iframe |
| 4 | Cmd/Ctrl-click `greet` on its second call | Editor jumps to the function definition (line 1); cursor lands on `greet` |
| 5 | Edit the file (insert a character) | `[data-testid="editor-pane-dirty"]` mounts with `aria-label="unsaved changes"` |
| 6 | Press Cmd/Ctrl+S | `editor-pane-dirty` unmounts; file on disk reflects the edit |
| 7 | Trigger an LSP error (kill the language server process mid-session) | `[data-testid="editor-pane-error"]` with `role="alert"` mounts; Reload action becomes visible |
| 8 | Click Reload | Error state clears; iframe re-attaches; diagnostics re-flow |

**Failure criteria:** iframe never reaches `editor-pane-iframe` state, diagnostics do not render, go-to-definition does not navigate, or the error state has no Reload affordance.

**Instrumentation gap:** Monaco internals (diagnostic line numbers, hover tooltip content) are queryable only by scraping the iframe DOM. Steps 2–3 may need an `editor-pane-diagnostic-count` data attribute exposed by the host wrapper; document as follow-up.

---

## UAT-03: TerminalPane — `forge-term` rendering, input, ANSI, resize

**Scope:** F-124 (`forge-term` crate: ghostty-vt wrapper + PTY I/O) + F-125 (TerminalPane + xterm.js + PTY IPC) + F-146 (libghostty-vt as authoritative VT state, zig CI + ghostty-vt wiring).
**Vehicle:** Playwright + `tauri-driver` (terminal subprocess spawned by `forge-shell`).

| Step | Action | Expected |
|------|--------|----------|
| 1 | Open a terminal pane | `[data-testid="terminal-pane-loading"][role="status"]` shows briefly, then `[data-testid="terminal-pane-host"]` mounts containing an `.xterm` element |
| 2 | Type `echo hello` and press Enter | Stdout `hello` renders in the host; cursor advances; xterm DOM contains the line |
| 3 | Type `printf '\x1b[31mred\x1b[0m\n'` | The word `red` renders with the red ANSI color (verify computed style on the rendered span) |
| 4 | Resize the pane (drag splitter) | xterm columns/rows recalc via `fitAddon.fit()`; `terminal_resize` IPC fires (spy via mocked layer) |
| 5 | Type `clear` and press Enter | xterm buffer resets; cursor returns to top |
| 6 | Trigger a spawn failure (set `$SHELL` to a nonexistent path before opening) | `terminal-pane-loading` clears to an error variant with `role="alert"` (see UAT-09 for the explicit selector gap) |

**Failure criteria:** terminal never spawns, ANSI escape codes render literally instead of as colors, `terminal_resize` is not called on resize, or the spawn-failure state crashes the pane.

**Instrumentation gap:** xterm.js manages its own DOM under `.xterm` without `data-testid`s. Cell-level assertions require xterm DOM scraping or postMessage instrumentation; document as follow-up.

---

## UAT-04: MCP import + tool invocation (stdio + http transports)

**Scope:** F-127–F-132 + F-155 (consolidation) + F-346 (SSRF guard) + F-347 (4 MiB frame cap) + F-348 (URL credential redaction).
**Vehicle:** bash + mock MCP servers.

Preparation:
```bash
# Build mocks (one-time; cached in target/).
cargo build -p forge-mcp --bin forge-mcp-mock-stdio --bin forge-mcp-mock-http
HTTP_PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")
"$REPO_ROOT/target/debug/forge-mcp-mock-http" --port "$HTTP_PORT" &
HTTP_PID=$!
trap "kill $HTTP_PID 2>/dev/null || true" EXIT

cat > "$WS/.mcp.json" <<EOF
{
  "mcpServers": {
    "stdio-mock": { "command": "$REPO_ROOT/target/debug/forge-mcp-mock-stdio", "args": [] },
    "http-mock":  { "url": "http://127.0.0.1:$HTTP_PORT/mcp" }
  }
}
EOF
```

| Step | Action | Expected |
|------|--------|----------|
| 1 | `forge mcp import "$WS/.mcp.json" --workspace "$WS"` | Both servers register; `$WS/.forge/mcp/registry.toml` (or equivalent) reflects two entries; stderr is silent |
| 2 | `forge mcp list --workspace "$WS"` | Both `stdio-mock` and `http-mock` appear with `state = healthy` after their initialize handshake |
| 3 | Spawn an agent: `forge run agent test-agent --workspace "$WS" --input - <<< "use the mock-tool tool"` | Agent emits a tool call to `mock-tool` exposed by the stdio server; stdout shows `ToolCallStarted` then `ToolCallCompleted` with the mock's canned result |
| 4 | Repeat with the http server's tool name | Same outcome via the http transport |
| 5 | Replace the http URL in `.mcp.json` with `http://10.0.0.1:8080/mcp` (private IP); re-import | `forge mcp import` exits non-zero with a `denied: SSRF guard rejects private address` style error per F-346; registry unchanged |
| 6 | Replace the http URL with `https://user:secret@example.com/mcp`; re-import (use a hostname that won't actually connect, just to test redaction); inspect logs | Registry entry's URL field stores `https://example.com/mcp` (credentials stripped); error logs render `https://[REDACTED]@example.com/mcp` per F-348 |
| 7 | Send a 4 MiB+ frame from the http mock | Connection terminates with `MalformedFrame` per F-347; mock client receives `ManagerEvent::HttpServerDegraded` (verify via `forge mcp list` or event log) |

**Failure criteria:** import succeeds for a private-IP URL (SSRF bypass), credentials leak into logs/registry, tools fail to surface from either transport, or 4 MiB frames hang/OOM the manager.

---

## UAT-05: Agents + sub-agents + AGENTS.md auto-injection + orchestrator routing

**Scope:** F-133 (`AgentInstance` + orchestrator API in `forge-agents`) + F-134 (sub-agent spawning via `agent.spawn` tool call + isolation enforcement) + F-135 (AGENTS.md auto-injection into system prompt) + F-136 (sub-agent banners in ChatPane) + F-352 (256 KiB AGENTS.md injection cap).
**Vehicle:** Playwright + `tauri-driver` + MockProvider scripting orchestrator/sub-agent turns.

Preparation:
```bash
mkdir -p "$WS/.agents"
cat > "$WS/.agents/orchestrator.md" <<'AGENT'
---
description: Orchestrator that delegates to specialists
---
You are the orchestrator. When asked to "do the thing", spawn the `worker` sub-agent.
AGENT

cat > "$WS/.agents/worker.md" <<'AGENT'
---
description: Worker sub-agent
---
You are a worker. Reply with "worker says: hello".
AGENT

cat > "$WS/AGENTS.md" <<'EOF'
# Repo conventions
Always greet politely.
EOF
```

| Step | Action | Expected |
|------|--------|----------|
| 1 | Open a session with `--agent orchestrator --workspace "$WS"`, click the session card | Session window opens; agent `orchestrator` shown in pane header |
| 2 | Send "do the thing" | Orchestrator emits a `spawn_sub_agent("worker", ...)` tool call; mock responds with sub-agent acknowledgement |
| 3 | Observe ChatPane | `[data-testid="sub-agent-banner-<child_id>"]` mounts; `[data-testid="sub-agent-banner-header-<id>"]` shows agent display name + model chip + tool count |
| 4 | Click the banner header | Banner expands; `[data-testid="sub-agent-banner-body-<id>"]` shows the sub-agent's transcript including its `worker says: hello` reply |
| 5 | Tab into the banner header, press Enter | Banner toggles via keyboard (focus management per F-138's a11y contract) |
| 6 | Verify AGENTS.md injection | Inspect orchestrator's first-turn system prompt (via a debug logging hook or by mocking the provider to echo its system prompt) — the prompt contains the contents of `$WS/AGENTS.md` |
| 7 | Generate an AGENTS.md > 256 KiB (e.g. `yes "x" \| head -c 300000 > "$WS/AGENTS.md"`); spawn a fresh session | Injection truncates at 256 KiB; warning surfaces per F-352 (CLI stderr or session event); orchestrator does not crash |
| 8 | Double-click the sub-agent banner header | Navigates to `/agent-monitor?instance=<child_id>` (continued in UAT-06) |

**Failure criteria:** sub-agent banner does not render, AGENTS.md is not injected, the 256 KiB cap is not enforced, or keyboard navigation does not toggle the banner.

**Instrumentation gap:** there is no `data-testid="agent-source"` indicating *which* `.agents/*.md` file backed an instance. Steps verifying agent provenance currently rely on the mock-provider's echo of system prompts; flagged for follow-up.

---

## UAT-06: Background agents + Agent Monitor (list, stop, status indicators)

**Scope:** F-137 (session orchestrator background-agent lifecycle + Tauri commands) + F-138 (status-bar background-agent indicator + completion notifications) + F-139 (fine-grained agent step events in `forge-session`) + F-140 (AgentMonitor three-column view) + F-152 (backend resource monitor for AgentMonitor pills: cpu/rss/fds) + F-153 (AgentMonitor entry points: command palette + status-bar badge nav) + F-156 (macOS + Windows resource samplers).
**Vehicle:** Playwright + `tauri-driver` + MockProvider.

| Step | Action | Expected |
|------|--------|----------|
| 1 | From a session, spawn a background agent (CLI-driven via `spawn_background_agent` tool call from the mock script) | Tool call card shows `spawn_background_agent` completed; instance id returned |
| 2 | Navigate to `/agent-monitor` | `[role="tablist"]` filter tabs render (`Active / All / Completed`); `[role="tabpanel"]` shows the new agent in a row with `[role="tab"]` ARIA wiring |
| 3 | Verify the row's progress bar | `.agent-monitor__progress[data-state="running"]` element present |
| 4 | Click the row | Inspector opens (`[aria-label="Inspector"]`); shows agent metadata, sampled CPU/memory series |
| 5 | Click the Stop button (`.agent-monitor__stop`) | `stop_background_agent` IPC fires; row's `data-state` flips to `done`; resource sampler untracks (verified via `agent_monitor_resources` IPC returning the row's id no longer in tracked set) |
| 6 | Spawn a second bg agent; switch the filter tab to `Completed` | First (stopped) agent appears; second (still running) does not |
| 7 | Press Escape with the inspector open | Step drawer (`role="dialog"`) closes if open; otherwise no-op |

**Failure criteria:** monitor row never appears, stop does not transition the row to terminal, resource sampler leaks (tracked count doesn't drop), or filter tabs do not gate the listed agents.

**Instrumentation gap:** "promote to foreground" — there is no externally-observable selector for promoting a background agent's instance window to foreground. If Phase 2 ships a promote affordance, add `data-testid="agent-promote"`. The current Inspector exposes Stop but the user-observable promote flow is not part of the closed Phase 2 issue set; documented as follow-up rather than a UAT failure.

---

## UAT-07: @-context picker — type `@`, browse, truncation notice, inject content

**Scope:** F-141 (ContextPicker component + `@`-trigger in composer) + F-142 (context category resolvers + provider adaptation) + F-147 (placement reconciliation: viewport-aware flip) + F-357 (`forge-fs` `list_tree` truncation signal in `TreeNodeDto.stats`; FilesSidebar's truncation-notice rendering ships under the same surface — see issue #536, F-357 follow-up).
**Vehicle:** Playwright + `tauri-driver`.

Preparation:
```bash
# Generate enough files to trigger the FS-walker truncation cap.
for i in $(seq 1 5000); do
  : > "$WS/file_$i.txt"
done
```

| Step | Action | Expected |
|------|--------|----------|
| 1 | Open a session, focus the chat composer | Composer is focused |
| 2 | Type `@` | `[data-testid="context-picker"][role="combobox"]` mounts; `aria-expanded="true"`, `aria-haspopup="listbox"` |
| 3 | Type `read` after `@` | `[data-testid="context-picker-query"]` reflects "read"; `[data-testid="context-picker-results"]` lists matching files |
| 4 | Verify truncation notice | `[data-testid="files-sidebar-stats-notice"]` (in the sidebar) AND `[data-testid="picker-truncation-notice"]` (inside the picker results panel, on the file/directory tabs) both show "N files not shown" reflecting the cap (cross-checked against `TreeNodeDto.stats.omitted_count`) |
| 5 | ArrowDown to navigate options | `aria-activedescendant` advances; visible selection moves |
| 6 | Tab to switch category to "Folder" | `[data-testid="context-picker-tab-folder"]` becomes active |
| 7 | Press Enter on a result | Picker closes; chat composer's text contains a chip referencing the picked file path |
| 8 | Submit the message | Provider receives the file's contents inlined into the prompt (verify via mocked-IPC spy) |
| 9 | Press Escape with picker open | Picker closes without inserting; composer focus restored |

**Failure criteria:** picker never opens on `@`, truncation notice missing when over cap (in either surface), ArrowDown/Tab navigation broken, Escape does not dismiss, or selected content is not injected.

**Instrumentation:** the inline truncation notice inside the picker results panel landed under issue #536 (`[data-testid="picker-truncation-notice"]`). It mirrors the FilesSidebar's wording ("N files not shown" / "tree truncated" / "N read errors") and `role="status"` ARIA pattern, and renders only on the file / directory tabs (the only categories backed by the `tree` IPC).

---

## UAT-08: Re-run Replace + Branch scaffolded

**Scope:** F-143 (Re-run Replace variant: truncate and regenerate) + F-144 (Re-run Branch variant: `branch_parent` threading + `BranchSelected` events) + F-145 (Branch UI: variant selector strip + gutter indicator + metadata popover).
**Vehicle:** Playwright + `tauri-driver` + MockProvider with re-run-aware scripting.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Send a prompt; mock responds with turn A | `[data-testid="branch-turn-<msg_id>"]` mounts containing the assistant message |
| 2 | Trigger Re-run / Replace from the message actions | New variant generated (turn A'); `[data-testid="branch-selector-strip"][role="group"]` mounts showing 2 variants |
| 3 | Inspect the strip | `[data-testid="branch-strip-prev"]`, `[data-testid="branch-strip-next"]`, `[data-testid="branch-strip-label"]` render with `aria-label` set |
| 4 | Click Next | Active variant flips to A'; ChatPane re-renders with A' visible (Replace semantics — the prior variant is replaced in the linear scroll, but kept in the strip's ring) |
| 5 | Click `[data-testid="branch-strip-info"]` | `[data-testid="branch-metadata-popover"]` opens with per-variant metadata |
| 6 | Locate the Branch button (per `SPECS.md` §15 it should be present but inert in Phase 2) | The control is rendered (visible / focusable) but clicking it produces no state change — neither dispatches a fork nor errors. Confirms the scaffold is in place for Phase 3 wiring |
| 7 | Press ArrowLeft / ArrowRight while strip is focused | Variant cycles through prev/next per F-160's keyboard contract |
| 8 | Close popover with Escape | Popover dismisses; focus returns to the info button |

**Failure criteria:** Re-run does not produce a new variant, selector strip never renders for a single-variant turn (it should mount the moment a second variant exists), Branch button is missing entirely or actively errors, or keyboard navigation does not cycle variants.

**Instrumentation gap:** the Branch button currently has no dedicated `data-testid` (`web/packages/app/src/routes/Session/ChatPane.tsx` `BranchedAssistantTurn`). Step 6 must locate it by accessible name for now; add `data-testid="message-branch-action"` when wiring for Phase 3.

---

## UAT-09: Loading / empty / error state coverage across panes

**Scope:** F-400 (interaction-state coverage in EditorPane, TerminalPane, FilesSidebar; closed in this milestone).
**Vehicle:** Playwright against Vite dev build with mocked IPC for deterministic state induction.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Mock `tree_load` to never resolve, mount FilesSidebar | `[data-testid="files-sidebar-loading"][role="status"]` visible |
| 2 | Mock `tree_load` to resolve with empty tree | `[data-testid="files-sidebar-empty"][role="status"]` visible |
| 3 | Mock `tree_load` to error | `[data-testid="files-sidebar-error"][role="alert"]` visible with retry |
| 4 | Mock the editor `monaco-host-ready` postMessage to never arrive, mount EditorPane | `[data-testid="editor-pane-loading"][role="status"]` visible |
| 5 | Trigger ready, then mock `file_read` to never resolve | `[data-testid="editor-pane-file-loading"][role="status"]` visible |
| 6 | Mock `file_read` to error | `[data-testid="editor-pane-error"][role="alert"]` visible with Reload |
| 7 | Mock the terminal spawn IPC to never resolve | `[data-testid="terminal-pane-loading"][role="status"]` visible |
| 8 | Mock terminal spawn to error | TerminalPane renders an error region with `role="alert"` (see Instrumentation gap) |
| 9 | Allow successful spawn after error | Loading clears, `[data-testid="terminal-pane-host"]` mounts |

**Failure criteria:** any of the four state transitions (loading → success, loading → empty, loading → error, error → recovery) does not surface its corresponding selector, OR any of those selectors lacks the `role="status"` / `role="alert"` ARIA contract for live-region announcements.

**Instrumentation gap:** TerminalPane's error container uses class-based selection (`.terminal-pane__error`) without a `data-testid`. Step 8 must scope by class for now; add `data-testid="terminal-pane-error"` to fully match the selector convention used by EditorPane and FilesSidebar.

---

## UAT-10: Security gates — SSRF on MCP import + sandbox-escape on rename/delete

**Scope:** F-346 (SSRF guard on HTTP MCP URLs), F-349 (Tauri commands no longer trust webview-supplied workspace_root).
**Vehicle:** bash + IPC harness.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Author `.mcp.json` with a private-IP HTTP server: `{"mcpServers":{"x":{"url":"http://192.168.1.1/mcp"}}}` | |
| 2 | `forge mcp import` it | Exits non-zero with `denied: SSRF guard rejects private address`; no registry mutation; no outbound connection attempted (verify with `tcpdump`/`ss` if needed) |
| 3 | Same with link-local `http://169.254.169.254/mcp` (cloud metadata IP) | Same denial |
| 4 | Same with `http://[::1]/mcp` (IPv6 loopback) | Denied |
| 5 | Drive the IPC layer directly (via a small test client or `forge` admin shell) and call `rename_path` with `from = "/etc/passwd", to = "/tmp/x"` claiming `workspace_root = "/etc"` | Call rejected with `forbidden: workspace_root` per F-349; `/etc/passwd` unchanged on disk |
| 6 | Same shape with `delete_path("/etc/passwd")` | Rejected; file unchanged |
| 7 | Cross-session probe: open session A and B; from session-B's window invoke `stop_background_agent({sessionId: "A"})` | Rejected with `forbidden: window label mismatch` (F-364 cross-session authz) |

**Failure criteria:** any private-IP URL imports successfully, any absolute-path rename/delete outside the legitimate workspace mutates disk, or cross-session calls succeed.

---

## UAT-11: Command palette (Cmd/Ctrl+Shift+P) — open, search, dispatch, dismiss

**Scope:** F-157 (Command palette infrastructure: registry API + fuzzy search + Cmd/Ctrl+Shift+P shortcut). Spec is `docs/ui-specs/command-palette.md` (added under F-414 in this milestone).
**Vehicle:** Playwright + `tauri-driver`.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Press Cmd/Ctrl+Shift+P from any session | Command palette dialog opens; focus moves into the search input; per `command-palette.md` §CP.1 a `role="dialog"` container mounts with focus trap |
| 2 | Type a partial command name (e.g. "tog") | Result list filters via fuzzy match per `command-palette.md` §CP.3; matching entries show with rank order |
| 3 | ArrowDown / ArrowUp | Selection cycles through results with arrow wrap per §CP.5; `aria-selected` reflects active row |
| 4 | Press Enter on a built-in command (e.g. "Toggle Files Sidebar") | Command dispatches; FilesSidebar visibility toggles; palette closes; focus restores |
| 5 | Re-open palette; type a query that matches nothing | Empty state row renders with `aria-disabled` per §CP.4; Enter no-ops |
| 6 | Press Cmd/Ctrl+Shift+P with palette open | Palette toggles closed (shortcut acts as toggle, not re-open) |
| 7 | Press Esc | Palette closes; focus restores to the previously-focused element |

**Failure criteria:** palette does not open on shortcut, fuzzy ranking is broken, selection wrap fails, dispatched command does not run, focus is dropped, or shortcut does not toggle.

---

## UAT-12: Persistent settings + persistent approvals

**Scope:** F-036 (persistent approval config) + F-151 (user + workspace settings store with persistence + Tauri IPC).
**Vehicle:** bash + Playwright.

Preparation:
```bash
# Settings store ships at $XDG_CONFIG_HOME/forge/settings.toml (user) and
# $WS/.forge/settings.toml (workspace). Approvals at $WS/.forge/approvals.toml.
unset FORGE_USER_CONFIG_DIR  # use default
```

| Step | Action | Expected |
|------|--------|----------|
| 1 | Launch a session, change a user-scoped setting via the Settings UI (e.g. theme, density) | `$XDG_CONFIG_HOME/forge/settings.toml` is created/updated atomically; the in-app value reflects the change |
| 2 | Quit and relaunch | Setting persists; UI reads the same value at startup |
| 3 | Run a tool call requiring approval; approve with the "This tool" scope | `$WS/.forge/approvals.toml` is created with an entry for the tool's whitelist scope |
| 4 | Quit, relaunch, run the same tool call | Tool auto-approves without prompting (whitelist read from disk on session start); the pill in the tool-call card reads `whitelisted · this tool` |
| 5 | Manually delete `$WS/.forge/approvals.toml` and rerun | Tool re-prompts; whitelist starts empty again |
| 6 | Inspect `$WS/.forge/settings.toml` after a workspace-scoped setting change | Workspace setting written to the workspace file, not the user file (verify keys are namespaced per `docs/architecture/persistence.md` if present, or per the F-151 DoD) |

**Failure criteria:** settings revert on relaunch, approvals do not persist across sessions, user vs. workspace scope conflated into a single file, or settings writes are non-atomic (corrupted on crash mid-write).

---

## Backend primitives covered by unit tests, not UATs

These Phase 2 deliverables have no user-visible surface or are pure invariants verified at the crate boundary. They get unit/integration tests, not UATs.

| Primitive | Ticket(s) | Where to run |
|-----------|-----------|--------------|
| MCP `http_roundtrip` flake — multi-signal drain | #561 / #562 | `cargo test -p forge-mcp --test http_roundtrip` |
| `stop_background_agent` registry-untrack + sampler-release | F-364 | `cargo test -p forge-shell --features webview-test --test ipc_bg_agents stop_background_agent_` |
| `rename_path` / `delete_path` sandbox-escape on absolute paths | F-364 | `cargo test -p forge-shell --features webview-test --test ipc_fs` |
| `stop_background_agent` cross-session authz | F-364 | included in `ipc_bg_agents` suite |
| MCP transport 4 MiB frame cap enforcement (parser-level) | F-347 | `cargo test -p forge-mcp transports` |
| MCP HTTP URL credential redaction (formatter-level) | F-348 | `cargo test -p forge-mcp redaction` |
| Atomic config-file write unification (single `write_atomic` helper) | F-372 | `cargo test -p forge-core atomic` |
| `forge-fs` `list_tree` truncation signal in `TreeNodeDto.stats` | F-357 | `cargo test -p forge-fs list_tree` |
| `forge-fs` `canonicalize_no_symlink` normalized-path acceptance | F-356 | `cargo test -p forge-fs canonicalize` |
| ts-rs trailing-spaces invariant in generated TS bindings | (test-infra) | `cargo test -p forge-ipc bindings` |
| AGENTS.md 256 KiB injection cap (in-process) | F-352 | `cargo test -p forge-agents agents_md_cap` |
| Typed-IPC wrapper enforcement (no string IPC names in production call-sites) | F-365 | `pnpm --filter app exec tsc --noEmit` (compile-time enforcement); `cargo test -p forge-ipc wrappers` |
| Listener lifecycle mount-race in AgentMonitor + TerminalPane | F-366 | component unit tests under `web/packages/app/src/...` |
| MCP manager error-path log levels (debug → warn) | F-368 | `cargo test -p forge-mcp manager` |
| MCP pump pending-table concurrent-recv | F-369 | `cargo test -p forge-mcp pump` |
| `useFocusTrap` ownership of menu paths; popover Esc scoping | F-402-followup | component tests |
| StatusBar accent-color WCAG AA reconciliation | F-392-followup | visual-regression / token-check |
| LSP HttpDownloader timeouts/size-cap/HTTPS-only/redirect policy | F-350 | `cargo test -p forge-lsp http_downloader` |
| Editor accent-color contrast on error copy | F-417-followup | visual-regression |
| Sandbox cgroup v2 PID controller (per-sandbox process limits) | F-149 | `cargo test -p forge-session sandbox` (cgroup mocked); production surface is enforcement-only, no UI |
| McpManager subprocess integration (serial CI job) | F-154 | `cargo test -p forge-mcp manager_integration -- --ignored` |
| macOS CI job: zig + cargo + rustdoc + tests + Tauri webview | F-158 | CI workflow file under `.github/workflows/`; not a runtime feature |
| cargo-audit + cargo-deny consolidation onto single `deny.toml` | F-115 | `cargo deny check`; not a runtime feature |
| Cross-platform process samplers (Linux verified end-to-end in UAT-06; macOS + Windows samplers exercised via per-platform CI) | F-156 | `cargo test -p forge-session resource_monitor --target <triple>` |

---

## Audit-children covered by their respective audit reports

Phase 2's four milestone-level audits — security (F-360), quality (F-390), frontend (F-419), docs (F-446) — produced ~78 follow-up issues whose fixes are **internal hardening** (no user-observable surface): security scanner bumps, log-level corrections, doc rewrites, token registrations, type-safety enforcement, parallel-API consolidations, etc. Per the audit-skill convention, each audit-child PR ships with its own unit/component test, the fix is verified by the audit-child issue's Definition of Done, and the consolidated audit-report issue is closed only when every child closes.

**Coverage chain — no per-issue UAT entry is needed:**

| Audit report | Children count | Verification mechanism |
|--------------|----------------|------------------------|
| F-360 (security) — closed | 15 | Each child PR carries its own crate-level test; consolidated report comment enumerates closed children by severity. `cargo audit` and `cargo deny check` enforce ongoing protection. |
| F-390 (quality) — closed | 29 | Same pattern; `pnpm typecheck`, `cargo clippy -D warnings`, `cargo fmt --check` enforce ongoing protection. |
| F-419 (frontend) — closed | 28 | Same pattern; `pnpm check-tokens`, component unit tests enforce ongoing protection. The user-observable surface of the frontend findings (CommandPalette spec, interaction-state coverage, ToolCallCard) is exercised by UAT-09 and UAT-11. |
| F-446 (docs) — closed | 26 | Same pattern; `lychee` (link-check) and ts-rs binding regeneration enforce ongoing protection. |

This deferral is structural: a milestone-level UAT plan that re-enumerated 78 audit-child DoDs would dilute the user-observable scenarios it exists to verify. The audit reports are the canonical record of those fixes.

---

## Known-gap follow-up

The "Known gap before reading further" section above lists Phase 2 deliverables whose user-observable surface is fine but whose docs/tooling artifacts have not landed:

- `CHANGELOG.md` — populate `[Unreleased]` from the closed-issue set before release-cut. **Release-blocker for `forge-milestone-release-readiness`.**
- `docs/architecture/ipc-contracts.md` — re-sync against the current Tauri command surface (the F-446 audit children listed the missing 28+ commands).
- `docs/architecture/crate-architecture.md` §3.3 — re-sync the Event variant list.
- `docs/build/approach.md`, `AGENTS.md` — remove `web/apps/` references.
- `web/packages/app/src/shell/StatusBar.css` — register the 19 `--fg-*` tokens in `tokens.css`, or refactor to reuse existing tokens.
- ADRs — author `docs/adrs/ADR-002-sandbox-cgroup.md` (F-149) and `docs/adrs/ADR-003-mcp-consolidation.md` (F-155).

These should be tracked as Phase 2 release-prep, not deferred to Phase 3.

---

## Pass / fail criteria

| Result | Definition |
|--------|-----------|
| **Pass** | All steps produce the expected outcome |
| **Fail** | Any step diverges; process crashes; state on disk wrong; security gate bypassed |
| **Blocked** | Prerequisites missing (`tauri-driver` not on path, mock MCP binaries not built) |

**Shippable bar — all must Pass:**
UAT-01 (outcome gate), UAT-04 (MCP import + invocation), UAT-05 (agents + sub-agents + AGENTS.md), UAT-06 (background agents + monitor), UAT-10 (security gates), UAT-12 (settings + approvals persistence).

**Stability bar — required before Phase 3 starts:**
UAT-02 (editor + LSP), UAT-03 (terminal), UAT-07 (@-context picker), UAT-08 (re-run replace), UAT-09 (state coverage), UAT-11 (command palette).

---

## Suggested harness layout

Mirror `docs/testing/phase1-uat.sh` for disk-state UATs (UAT-04, UAT-10) and Phase 1's Playwright invocation pattern for the GUI UATs. Place Playwright specs under `web/packages/app/tests/phase2/` (one spec per UAT, named `uat-NN-<slug>.spec.ts`) and wire `pnpm --filter app test:e2e:phase2` to:

1. Build the app (`pnpm --filter app build`) and the Tauri shell in debug mode.
2. Build the MCP mocks (`cargo build -p forge-mcp --bin forge-mcp-mock-stdio --bin forge-mcp-mock-http`).
3. Start `tauri-driver` for `tauri-driver`-backed specs (UAT-01, UAT-02, UAT-03, UAT-05, UAT-06, UAT-07, UAT-08, UAT-11, UAT-12 GUI portion).
4. Start Vite dev for mocked-IPC specs (UAT-09).
5. Run `playwright test --project=phase2`.

A companion `docs/testing/phase2-uat.sh` orchestrates the bash UATs (UAT-04, UAT-10) and invokes the Playwright pnpm script, matching Phase 1's single-entry-point UX.

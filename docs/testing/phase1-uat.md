# Phase 1 User Acceptance Test Plan

**Scope:** Phase 1: Single Provider + GUI — Dashboard, Session window, chat pane, tool calls, four-scope approval, archive-on-end.
**Outcome gate:** A user can launch Forge, open a session from the Dashboard, exchange messages (with streaming) against MockProvider, trigger a tool call, approve it inline, and see the result rendered in the chat pane.

> **Post-Phase-3.1 note.** The Phase-1 milestone originally shipped a dedicated `OllamaProvider`, an Ollama status card on the Dashboard, and `ollama:<model>` provider selection. Those surfaces were retired in Phase 3.1 in favour of a `custom_openai` preset, so the Ollama-specific UATs (UAT-01b, UAT-01c, UAT-03, and UAT-12 variant A) have been dropped from this plan. The remaining MockProvider-backed UATs still document the Phase-1 outcome gate.

> **Authoring convention:** new UATs in this plan should follow the `contract-level` / `acceptance-only` labelling defined in [`docs/testing/uat-conventions.md`](./uat-conventions.md). The convention's migration table tentatively classifies each Phase 1 UAT below; back-labelling each scenario in place is tracked under F-327 follow-up. The `contract-level` set feeds the persistent smoke-UAT suite (F-326).

---

## Provider selection

`forged` selects its provider from (in precedence order) the `--provider <spec>` CLI flag, the `FORGE_PROVIDER` env var, or — when neither is set — defaults to `MockProvider`. Spec grammar:

| Spec | Effect |
|------|--------|
| `mock` | MockProvider (uses `FORGE_MOCK_SEQUENCE_FILE` if set, else default path) |

UAT-01a runs against MockProvider. See `phase1-uat-setup.md` for the harness env vars.

---

## What this plan covers

Phase 1 ships 14 feature tickets (F-018 — F-031) plus one docs-only ticket (F-032). Rather than restate each ticket's Definition of Done as a UAT (those are already enforced by unit tests), the cases below verify **user-observable behavior** that depends on multiple tickets working together. Backend primitives with no user surface in Phase 1 (e.g. `SandboxedCommand`, tool dispatcher registration) are called out at the bottom and are covered by crate-level unit tests, not by UATs.

Automation vehicle:
- **GUI UATs** — Playwright driving the Tauri app via `tauri-driver`, with a Vite-dev mocked-IPC harness for component-level cases that don't need the full shell.
- **Disk / state UATs** — bash harness invoking `forge` / `forged` and inspecting the filesystem, in the same style as `docs/testing/phase0-uat.sh`.

---

## Prerequisites

| Item | Requirement |
|------|-------------|
| Rust build | `cargo build --workspace` succeeds |
| Binaries | `forge`, `forged`, and `forge-shell` on `$PATH` or under `target/{debug,release}` |
| Web build | `pnpm install && pnpm --filter app build` from `web/` succeeds |
| Design tokens | `pnpm check-tokens` from `web/` passes (runs `scripts/check-tokens.mjs` per `docs/frontend/generation-pipelines.md`; guards F-018 drift) |
| Playwright | `pnpm --filter app exec playwright install` has been run |
| Tauri driver | `tauri-driver` available on `$PATH` (`cargo install tauri-driver`) |
| Mock provider | `FORGE_MOCK_SEQUENCE_FILE` points to a JSON array of NDJSON scripts (see `docs/testing/phase0-uat.sh` lines 80-90 for the format). Consumed by `forged` per `crates/forge-session/src/main.rs:38-45`. |
| Mock agent | `.agents/test-agent.md` exists in the scratch workspace |
| Workspace | An empty temp directory per run (no pre-existing `.forge/`) |
| Roadmap (F-032) | `docs/build/roadmap.md` Phase 1 / Phase 3 sections match the GitHub milestone descriptions — verify with a `diff` in CI or by hand before running UATs |

---

## UAT-01a: Outcome gate — launch, chat, tool call, approve (MockProvider)

**Scope:** F-019 + F-020 + F-022 + F-024 + F-025 + F-026 + F-027 + F-028 + glue.
**Vehicle:** Playwright + `tauri-driver`, `forged` driven by MockProvider via `FORGE_MOCK_SEQUENCE_FILE`.
**Why this test exists:** this is the Phase 1 outcome statement restricted to what has actually been wired end-to-end. If UAT-01a passes, the full UI → IPC → session → tool-call → approval → chat pipeline shipped.

Preparation — write a mock script with three scripted turns: plain text, a `fs.read` tool call, then a continuation:

```bash
cat > "$WS/mock.json" <<'EOF'
[
  "{\"delta\":\"Looking at your workspace…\"}\n{\"done\":\"end_turn\"}",
  "{\"delta\":\"I will read the file.\"}\n{\"tool_call\":{\"name\":\"fs.read\",\"args\":{\"path\":\"readable.txt\"}}}\n{\"done\":\"tool_use\"}",
  "{\"delta\":\"Done reading.\"}\n{\"done\":\"end_turn\"}"
]
EOF
export FORGE_MOCK_SEQUENCE_FILE="$WS/mock.json"
echo "hello from forge phase1 UAT" > "$WS/readable.txt"
```

| Step | Action | Expected |
|------|--------|----------|
| 1 | Seed: `forge session new agent test-agent --workspace $WS` | Session id printed; socket + `events.jsonl` created with `{"schema_version":1}` first line |
| 2 | Launch: `cargo run -p forge-shell` (or built bundle) | Dashboard opens; the seeded session appears on the Active tab within 2s |
| 3 | Click the session card | Session window opens; pane header shows subject, provider label ("Mock" or configured default), cost meter placeholder, close action |
| 4 | Type `read the file` and press Enter | Composer clears and disables; blinking cursor appears on the assistant bubble |
| 5 | Observe chat pane | Tokens from the first mock turn stream in; auto-scroll pins to bottom |
| 6 | Send a second prompt `go ahead` to advance to turn 2 | Tool call card renders inline for `fs.read readable.txt`; status is **awaiting approval** |
| 7 | Press `A` with the card focused (Approve Once) | Card transitions to in-progress → completed; expanded state shows the file contents as the result preview |
| 8 | Observe remaining output | Turn 3 streams; composer re-enables when `AssistantMessage(final)` arrives |
| 9 | Close the Session window | Subscribe listener detaches (no warnings in webview console); Dashboard still shows the session on Active |

**Failure criteria:** streaming tokens never arrive, the tool call card never renders, approval appears as a modal, the result preview is missing, or the composer does not re-enable.

---

## UAT-02: Dashboard sessions list

**Scope:** F-022.
**Vehicle:** Playwright against Vite dev build with mocked `session_list` IPC.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Mock `session_list` to return `[]` | Dashboard Active tab shows an empty-state message per `docs/design/voice-terminology.md` |
| 2 | Mock it to return three active + two archived sessions (subject, provider, last_event_at, persistence, state varied) | Active tab renders three cards with subject, provider label, last-activity timestamp, persistence badge (persist / ephemeral) |
| 3 | Switch to Archived tab | Two archived cards render; Active count in tab header updates |
| 4 | Mock a session with a stale socket (`state: "stopped"`) | That card renders with a stopped-state visual distinct from active |
| 5 | Click an Active card | `open_session(id)` invoked with the correct id (spy on IPC) |
| 6 | Click an Archived card | Read-only affordance (no `open_session` for archived in Phase 1 — reactivation is deferred per F-031 scope) |

**Failure criteria:** tabs do not switch, clicking active card does not invoke `open_session`, or visual badges are missing.

---

## UAT-04: Session window lifecycle

**Scope:** F-024.
**Vehicle:** Playwright with mocked IPC spy.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Open a Session window | IPC spy sees exactly one `session_hello` followed by one `session_subscribe` for that session id |
| 2 | Inspect the pane header | Subject, provider label, cost meter placeholder, close action visible; matches `docs/ui-specs/layout-panes.md` §3.4 |
| 3 | Confirm no split UI | No splitter, no dock zones (splits are deferred to Phase 2) |
| 4 | Close the window | IPC spy sees `session_subscribe` listener detached; no further `session:event` emissions reach the window |
| 5 | Re-open the same session | `session_hello` fires once more (reuses UDS connection per F-020), subscription re-established |

**Failure criteria:** subscribe fires twice on mount, listener leaks after close, or a split control is visible.

---

## UAT-05: Chat pane streaming and composer

**Scope:** F-025.
**Vehicle:** Playwright against a real `forge-session` backed by `MockProvider` (deterministic streaming + tool calls).

| Step | Action | Expected |
|------|--------|----------|
| 1 | Send "hello" by pressing Enter | Composer clears, disables, and the assistant bubble appears with a blinking cursor |
| 2 | During stream | Tokens accumulate in order; no flicker |
| 3 | Press Shift+Enter while composing a second message | Newline inserted into composer; message not sent |
| 4 | Scroll the chat pane upward mid-stream | Auto-scroll pin releases; new tokens append off-screen without snapping the view |
| 5 | Scroll back to the bottom | Pin re-engages; new tokens keep the view at the bottom |
| 6 | Trigger an error event from the mock | Inline error renders with voice-compliant copy (per `docs/design/voice-terminology.md`) |
| 7 | Wait for `AssistantMessage(final)` | Cursor disappears; composer re-enables |

**Failure criteria:** composer stays enabled during streaming, auto-scroll pin does not release on user scroll, Shift+Enter submits, or errors render as toasts/modals rather than inline.

---

## UAT-06: Tool call card rendering

**Scope:** F-026.
**Vehicle:** Playwright + MockProvider (can script any `ToolCallStarted`/`ToolCallCompleted` pairs).

| Step | Action | Expected |
|------|--------|----------|
| 1 | Mock drives a single `fs.read` tool call to completion | Collapsed card shows: icon, `fs.read`, one-line path arg, duration, status glyph |
| 2 | Click the card | Expands to full args JSON + content preview + metadata (bytes, sha) |
| 3 | Click again | Collapses; expanded state persists while the window is open, not across windows |
| 4 | Mock drives three `fs.read` calls sharing a `batch_id` | Renders under a single `ToolCallGroup` header with "3 read-only tool calls" summary; group toggle collapses all three |
| 5 | Mock a call that errors (`ToolCallCompleted` with error) | Card shows errored status using error color token from `docs/design/color-system.md` |
| 6 | Mock an `fs.edit` completion with diff | Expanded state shows a unified-diff result preview |

**Failure criteria:** parallel calls render as three separate cards, expand toggle does not persist, or status glyphs do not map to the four states (awaiting-approval, in-progress, completed, errored).

---

## UAT-07: Four-scope inline approval

**Scope:** F-027 (depends on F-026, F-028, F-029).
**Vehicle:** Playwright + MockProvider scripting write-class tool calls.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Mock drives `fs.edit` on `src/a.ts` — approval required | Tool call card expands automatically; approval prompt renders **inline inside the card** (never as a modal) |
| 2 | Inspect prompt | Reject button + Approve split-button with four scopes (Once, This file, This pattern, This tool) visible per `docs/ui-specs/approval-prompt.md` |
| 3 | Inspect diff preview | Unified-diff text from `ApprovalPreview` renders in the card body |
| 4 | Press `R` with card focused | Call rejected; agent receives rejection; card shows errored/cancelled state |
| 5 | Repeat with a new `fs.edit` on `src/a.ts`; press `A` | Call approved once; next `fs.edit` on `src/a.ts` re-prompts |
| 6 | Mock third `fs.edit` on `src/a.ts`; press `F` | Call approved; subsequent `fs.edit` calls on `src/a.ts` **auto-approve** and render a `whitelisted · this file` pill in the card header |
| 7 | Mock `fs.edit` on `src/b.ts` | Re-prompts (pattern did not match file scope) |
| 8 | On that prompt press `P`, edit pattern to `src/*`, confirm | Subsequent `fs.edit` calls under `src/*` auto-approve with `whitelisted · pattern src/*` pill |
| 9 | Mock `fs.write` anywhere; press `T` | All subsequent `fs.write` calls auto-approve with `whitelisted · tool` pill |
| 10 | Mock `shell.exec` | Preview shows command + cwd per `docs/ui-specs/approval-prompt.md` §10.3 |

**Failure criteria:** any approval rendered as a modal, keyboard shortcut firing on unfocused card, or whitelist pill absent after scope approval. (Cross-session whitelist leak is verified in UAT-11.)

---

## UAT-08: fs.write / fs.edit through the GUI

**Scope:** F-028 + F-029 surfaced via F-027.
**Vehicle:** Playwright + real `forge-session` + MockProvider scripting the tool-call turns. Real filesystem under a tempdir workspace.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Mock drives `fs.write` with `path` outside `allowed_paths` (e.g. `/etc/passwd`) | Approval prompt **still renders first** — `forge-fs` enforces `allowed_paths` inside `invoke()` (`crates/forge-fs/src/mutate.rs:48-51`), not during preview. Approve Once, then observe: error event with `FsError::PathDenied` surfaces in chat; `/etc/passwd` unchanged on disk |
| 2 | Mock drives `fs.write` inside the workspace, approve Once | Result preview shows bytes written; `readable.txt` exists on disk with expected contents |
| 3 | Mock drives `fs.edit` on that file with a valid unified-diff patch, approve Once | Diff preview rendered in approval, file updated on disk; expanded card shows final diff |
| 4 | Mock drives `fs.edit` with a malformed patch | Error event in chat; file on disk unchanged |
| 5 | Mock drives `fs.edit` on a nonexistent file | Error event in chat; no file created |

**Failure criteria:** any disk write outside the allowed paths, malformed patch silently succeeds, or the invoke-time denial is not surfaced as an inline error event.

**Note on step 1 UX:** the approval-then-deny sequence is awkward user experience. If a reviewer wants upfront path denial in the preview, that's a new ticket — F-029's DoD only specifies `PathDenied` is returned by `invoke()`.

---

## UAT-09: Persist session archive on end

**Scope:** F-031 + F-022 (Archived tab).
**Vehicle:** bash harness.

| Step | Action | Expected |
|------|--------|----------|
| 1 | `forge session new agent test-agent --workspace $WS` (persist is the default for `session new agent`) | Session spawned; `$WS/.forge/sessions/<id>/events.jsonl` exists |
| 2 | Exchange a message via `forge session tail` / UDS helper | Events appended |
| 3 | `forge session kill <id>` (or session ends naturally) | |
| 4 | Inspect disk | `$WS/.forge/sessions/<id>/` no longer exists; `$WS/.forge/sessions/archived/<id>/events.jsonl` exists; first line is `{"schema_version":1}` |
| 5 | Read `$WS/.forge/sessions/archived/<id>/meta.toml` | `state = "archived"`; `ended_at` is RFC 3339 in the last 5 seconds |
| 6 | Check runtime dir | `$FORGE_RUNTIME_DIR/forge/sessions/<id>.sock` removed |
| 7 | Open Dashboard; switch to Archived tab | The session appears; Active tab does not show it |

**Failure criteria:** session dir still under `sessions/` after end, meta not rewritten, socket file lingers, or archived session does not appear on the Dashboard.

---

## UAT-10: Ephemeral session purge on end

**Scope:** F-031.
**Vehicle:** bash.

| Step | Action | Expected |
|------|--------|----------|
| 1 | `echo "hello" \| forge run agent test-agent --input -` (Phase 1's ephemeral path; `forge run` forwards `--ephemeral` to `forged`) | Session runs headless, streams events to stdout, exits 0 |
| 2 | Session exits on `SessionEnded` | |
| 3 | Inspect disk | `$WS/.forge/sessions/<id>/` entirely removed; nothing under `archived/` either |
| 4 | Check socket | `$FORGE_RUNTIME_DIR/forge/sessions/<id>.sock` removed |
| 5 | Open Dashboard | Session does not appear on Active or Archived tabs |

**Failure criteria:** any remnant on disk, socket not cleaned, or session visible on Dashboard.

---

## UAT-11: Multi-session isolation

**Scope:** F-020 IPC bridge, F-022 open, F-024 window, F-025 pane, F-027 whitelist.
**Vehicle:** Playwright + `tauri-driver`, two sessions backed by MockProvider.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Spawn sessions A and B via CLI; both appear on Dashboard | |
| 2 | Open A and B in separate Session windows | Both render independently; no events cross |
| 3 | Send a message in A while B streams | A's tokens appear only in A; B's in B |
| 4 | Trigger a tool call needing approval in A; approve `This tool` scope | Subsequent same-tool calls in A auto-approve |
| 5 | Trigger the same tool in B | B **re-prompts**; whitelists are per-session, not global |
| 6 | Close A | B continues uninterrupted |

**Failure criteria:** any event from one session renders in the other, or whitelists leak across sessions.

---

## UAT-12: Recovery — daemon disappears mid-stream

**Scope:** IPC bridge + chat pane resilience.
**Vehicle:** Playwright + signal-kill.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Start a session; begin streaming | |
| 2 | `kill <forged-pid>` | Session window surfaces a disconnect indicator; no unhandled promise rejection in console |
| 3 | Return to Dashboard | Session state reads `stopped` |

**Failure criteria:** window crashes, unhandled exceptions in the webview console, or no user-visible signal that the stream died.

---

## UAT-13: CLI / GUI parity spot check

**Scope:** Invariant "No GUI-only features" from `AGENTS.md`.
**Vehicle:** bash + Playwright.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Spawn three sessions (mix of persist + ephemeral); end one | |
| 2 | Run `forge session list` and capture output | |
| 3 | Open Dashboard Active tab | Same set of sessions (by id, subject, persistence) as the CLI lists as active |
| 4 | Switch to Archived tab | Matches the CLI's archived listing |

**Failure criteria:** any session present in the GUI but not the CLI (or vice versa) at the same point in time.

---

## UAT-14: Webview CSP enforcement

**Scope:** F-050 (restrictive production Content-Security-Policy, replaces the null policy that shipped with the webview bootstrap).
**Vehicle:** Playwright spec at `web/packages/app/tests/phase1/uat-csp-meta.spec.ts` (authored alongside F-050; this entry documents the coverage that was previously orphaned from the plan).

| Step | Action | Expected |
|------|--------|----------|
| 1 | Load the production-built webview | Response headers / `<meta http-equiv="Content-Security-Policy">` contain the restrictive directive set defined in `forge-shell` Tauri config |
| 2 | Inject a fixture `<script>alert('xss')</script>` via a DOM attack vector | Browser blocks execution per CSP; no `alert` dialog; console reports the CSP violation |
| 3 | Attempt an inline-style attack (`<div style="...">` with fixture JS expression) | Blocked by CSP; element renders without the style |
| 4 | Attempt to fetch an off-allowlist remote origin | Blocked; network tab shows the CSP refusal |

**Failure criteria:** any injected payload executes; any off-allowlist origin loads; CSP directive drifts from the canonical list in `docs/architecture/security.md`.

---

## UAT-15: ApprovalPrompt accessibility

**Scope:** F-088 (`role="alertdialog"` + `aria-live`), F-089 (focus trap + focus restoration). Shipped as part of the Phase 1 frontend polish; this entry makes the accessibility contract explicit after the Phase 1 frontend review flagged it as plan-undocumented.
**Vehicle:** Playwright + axe-core assertions against the Tauri webview with MockProvider scripting an approval-required tool call.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Mock drives `fs.edit` on `src/a.ts`; approval prompt renders inline in the card | Prompt container exposes `role="alertdialog"` with an accessible name (F-088); an `aria-live` polite region announces the prompt to screen readers |
| 2 | Inspect keyboard focus | Focus lands inside the prompt (first actionable control) automatically when the prompt renders (F-089) |
| 3 | `Tab` / `Shift+Tab` repeatedly | Focus cycles only within the prompt — focus trap keeps Tab from escaping back to the chat list or composer (F-089) |
| 4 | `Esc` or Reject | Prompt closes; **focus returns to the element that held it before the prompt opened** (F-089) |
| 5 | Run axe-core on the session window with an active prompt | Zero violations in the prompt container |

**Failure criteria:** missing `role="alertdialog"` / `aria-live`, Tab escapes the prompt, focus is dropped or restored to an unrelated element, or axe-core reports any violation inside the prompt.

---

## Primitives covered by unit tests only (not UATs)

These Phase 1 deliverables have no user-visible surface yet. Verification is at the crate level via `cargo test`; they do not get UATs in this plan.

| Primitive | Ticket | Where to run |
|-----------|--------|--------------|
| `SandboxedCommand` (env whitelist, rlimit, pgid kill) | F-030 | `cargo test -p forge-session sandbox` — `shell.exec` is a stub in Phase 1; real surface arrives in a later phase |
| Tool dispatcher registration / collision / unknown-tool | F-028 | `cargo test -p forge-session tools` |
| `forge_fs::write_preview` / `edit_preview` output shape | F-029 | `cargo test -p forge-fs` |
| `archive_or_purge` cross-device rename fallback | F-031 | `cargo test -p forge-session archive` |
| Roadmap doc sync | F-032 | Textual `diff` of `docs/build/roadmap.md` vs GitHub milestone descriptions in CI (prereq, not a UAT) |
| UDS socket auth / permissions (`XDG_RUNTIME_DIR` requirement, `0o600` socket / `0o700` parent, TOCTOU-safe unlink) | F-044, F-056 | `cargo test -p forge-session uds_socket_permissions uds_bind_symlink_race socket_path_fallback` — backend trust-boundary invariant, no user-facing surface for UAT |

---

## Pass / fail criteria

| Result | Definition |
|--------|-----------|
| **Pass** | All steps produce the expected outcome |
| **Fail** | Any step diverges; process crashes; state on disk wrong |
| **Blocked** | Prerequisites missing (`tauri-driver` not on path) |

**Shippable bar — all must Pass:**
UAT-01a (MockProvider outcome gate), UAT-02 (sessions list), UAT-05 (chat streaming), UAT-07 (four-scope approval), UAT-08 (fs.write / fs.edit through approval), UAT-09 (persist archive).

**Stability bar — required before Phase 2 starts:**
UAT-04, UAT-06, UAT-10, UAT-11, UAT-12, UAT-13.

---

## Suggested harness layout

Mirror `docs/testing/phase0-uat.sh` for disk-state cases (UAT-09, UAT-10, UAT-13). Place Playwright specs under `web/packages/app/tests/phase1/` (one spec file per UAT, named `uat-NN-<slug>.spec.ts`) and wire a `pnpm --filter app test:e2e` script that:

1. Builds the app (`pnpm --filter app build`) and the Tauri shell in debug mode.
2. Starts `tauri-driver` for `tauri-driver`-backed specs (UAT-01a, UAT-11, UAT-12).
3. Starts Vite dev for mocked-IPC specs (UAT-02, UAT-04 – UAT-08).
4. Runs `playwright test` with the appropriate project configuration.

A companion `docs/testing/phase1-uat.sh` can orchestrate the bash-only UATs and invoke the Playwright pnpm script, matching Phase 0's single-entry-point UX. The script itself is a follow-up deliverable to this plan.

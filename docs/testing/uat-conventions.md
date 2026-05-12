# UAT Authoring Conventions

**Status:** Active.
**Audience:** Anyone authoring or editing a `docs/testing/phaseN-uat.md` plan.
**Related work:** F-326 (persistent smoke-UAT suite — consumes the `contract-level` set defined here), F-327 (this convention).

---

## Why this exists

Per-milestone UAT plans (`docs/testing/phaseN-uat.md`) currently mix two very different kinds of scenarios:

1. Tests that pin a **durable backend, CLI, or IPC contract** which subsequent phases continue to depend on (e.g. the JSONL event-log schema header, UDS frame validation, the `forge mcp import` command surface).
2. Tests that pin **milestone-specific UI acceptance** that may legitimately change or be removed in a later phase (e.g. "Dashboard Active tab renders three cards", "drag the EditorPane title-bar onto the right-half drop zone").

Without distinguishing the two, every UAT is ambiguous about whether it should keep running in later phases. The default today is "the prior phase's UATs are not re-run," which loses regression coverage on durable surfaces. F-326 builds a persistent smoke-UAT suite to fix that — but the suite needs an unambiguous, machine-extractable signal for which scenarios to include. This doc defines that signal.

---

## The two labels

Every UAT scenario MUST carry exactly one of:

### `contract-level`

Tests a backend, CLI, or IPC contract that is **expected to remain stable across phases**. If this scenario stops passing in a later phase, that's a regression on a durable surface and the answer is almost always "fix the regression," not "retire the test."

Rule of thumb: *"Would users reasonably complain if this exact behaviour stopped working in a future phase?"* If yes, label `contract-level`.

Typical examples:

- Append-only event-log file format and schema header.
- UDS protocol frame validation, version negotiation, size cap.
- CLI argument parsing and exit codes for stable subcommands.
- `forge mcp import` SSRF guard, credential redaction, frame-cap enforcement.
- Sandbox-escape rejection on `rename_path` / `delete_path`.
- Cross-session IPC authz checks.

### `acceptance-only`

Tests a **milestone-specific UI flow or feature** that is not promised to outlive its origin phase. Provides evidence that the feature shipped correctly, but does not need to keep passing once the phase closes.

Rule of thumb: *"Would a user legitimately expect this exact behaviour to change in a future phase?"* If yes (e.g. the Dashboard layout is going to be redesigned in Phase 3), label `acceptance-only`.

Typical examples:

- Specific Dashboard tab layouts, card visuals, badge placements.
- Drag-to-dock interactions on a particular pane combination.
- A specific component's keyboard map or focus order.
- A particular wording of an inline error message.

---

## How to apply the label

Add a `**Type:**` line directly under each `## UAT-NN: ...` heading, before the `**Scope:**` / `**Vehicle:**` lines. Use one of the two literal values:

```markdown
## UAT-09: Event Log Durability and Replay

**Type:** contract-level
**Scope:** Append-only log with schema header; replay on reconnect.
**Vehicle:** bash.

| Step | Action | Expected |
| ... |
```

```markdown
## UAT-02: Dashboard sessions list

**Type:** acceptance-only
**Scope:** F-022.
**Vehicle:** Playwright against Vite dev build with mocked `session_list` IPC.

| Step | Action | Expected |
| ... |
```

Rules:

- Exactly one `**Type:**` line per UAT scenario.
- Value MUST be the literal string `contract-level` or `acceptance-only` (lowercase, hyphenated, no surrounding code fences). Future tooling (F-326) extracts these by exact match.
- Place it as the **first** metadata line under the heading so the label is impossible to miss when reading or grepping.
- If a scenario genuinely covers both a durable contract and a milestone-only UI flow, split it into two scenarios (`UAT-NNa` / `UAT-NNb`) rather than dual-labelling. The persistent suite cannot run "half a UAT."
- Ambiguous cases — flag them in the PR description rather than guessing. A reviewer's call is preferable to a misclassification that quietly drops contract coverage.

---

## How the harness consumes the label

> **Status:** implemented in F-326. Each `docs/testing/phaseN-uat.sh` accepts `--contract-only`, and `docs/testing/smoke-uat.sh` aggregates them.

The phase harness scripts (`docs/testing/phaseN-uat.sh`) accept a single flag:

```bash
./docs/testing/phase2-uat.sh --contract-only
```

Behaviour:

- With no flag: runs the phase's full UAT set as today (both label types).
- With `--contract-only`: filters to scenarios whose `**Type:**` line equals `contract-level`, regardless of phase.

The persistent smoke-UAT runner (F-326) is conceptually `for each phaseN-uat.sh; phaseN-uat.sh --contract-only`. The runner does not duplicate scenarios into a separate file — the per-phase plan stays the canonical source. This avoids classification drift between two copies of the same scenario.

Implementation note: the `**Type:**` line is the **specification** of which scenarios are contract-level. Each phase script encodes the same set as a `CONTRACT_LEVEL_UATS=(UAT-NN ...)` array near the top of the file; the array is the **executable** form of the same classification. When you change a label in the markdown, update the array in the matching phase script in the same PR. F-326's smoke runner relies on the two staying in sync.

---

## The persistent smoke-UAT suite (F-326)

The persistent suite lives at `docs/testing/smoke-uat.sh`. It exists so that contract-level scenarios from older phases continue to be exercised on every release-cut and (eventually) every main-commit CI build, instead of going stale until someone re-runs the originating phase's full UAT.

### How to run it

```bash
# Full sweep (Phases 0, 1, 2; bash + Playwright):
./docs/testing/smoke-uat.sh --build

# Without rebuilding (assumes binaries + web build already exist):
./docs/testing/smoke-uat.sh

# Only the bash-driven contract-level UATs (no Playwright):
./docs/testing/smoke-uat.sh --cli-only

# Only the Playwright-driven contract-level UATs (skips Phase 0):
./docs/testing/smoke-uat.sh --gui-only

# Subset of phases (e.g. skip Phase 1):
./docs/testing/smoke-uat.sh --phases 0,2
```

The runner forwards `--contract-only` to every selected `phaseN-uat.sh` and aggregates pass/fail/skip counts. It exits non-zero if any phase script exits non-zero.

### What it covers

The suite runs the union of all `contract-level` UATs across every existing phase plan. The current contents are sourced from the [migration tables](#migration-classifying-the-existing-phase-0--1--2-uats) above:

- **Phase 0** — all 13 UATs (`UAT-01` .. `UAT-13`).
- **Phase 1** — `UAT-08`, `UAT-09`, `UAT-10`, `UAT-11`, `UAT-13`, `UAT-14`.
- **Phase 2** — `UAT-04`, `UAT-05`, `UAT-10`, `UAT-12`.

If you need the executable enumeration (e.g. for CI matrix logic), grep the `CONTRACT_LEVEL_UATS=` array in the matching `phaseN-uat.sh`.

### When to run

- Before every release-cut, in addition to the new milestone's full phase UAT.
- Ideally on every `main` push in CI. (Wiring this into the GitHub Actions workflow is a follow-up — the suite is intentionally a single shell entry point so the CI step is one line.)

### Adding new contract-level UATs

When authoring a new milestone's UAT plan:

1. Apply the `**Type:** contract-level` label per the [How to apply the label](#how-to-apply-the-label) section.
2. Add the scenario's UAT-id to the matching `CONTRACT_LEVEL_UATS=(...)` array in the new `phaseN-uat.sh`. Include the scenario in `CONTRACT_LEVEL_GUI_FILTER` (a `|`-joined Playwright filename pattern) if it is a GUI scenario.
3. The `smoke-uat.sh` runner will pick the scenario up automatically — no edits to the runner are required.
4. The convention doc's [migration tables](#migration-classifying-the-existing-phase-0--1--2-uats) are a tentative starting point; the executable source of truth for any phase is its script's `CONTRACT_LEVEL_UATS` array.

---

## Migration: classifying the existing Phase 0 / 1 / 2 UATs

This section is a starting point, not the final source of truth. The PR that back-labels each phase plan in place will revise these as needed, and any disagreement should be resolved on that PR rather than by editing this table.

### Phase 0 (`docs/testing/phase0-uat.md`)

| UAT | Tentative label | Rationale |
|-----|-----------------|-----------|
| UAT-01 Spawn a Session | contract-level | Session bootstrap files (socket, PID, event-log header) are durable. |
| UAT-02 List Sessions | contract-level | `forge session list` output schema is part of the CLI contract. |
| UAT-03 Tail Event Stream | contract-level | Replay + live tail is a durable IPC surface. |
| UAT-04 Kill a Session | contract-level | Lifecycle + cleanup invariants. |
| UAT-05 Send Message / Receive Response | contract-level | UDS frame + event ordering. |
| UAT-06 Tool Call Approval Gate | contract-level | Approval protocol is durable. |
| UAT-07 Tool Call Auto-Approve Mode | contract-level | `--auto-approve-unsafe` flag contract. |
| UAT-08 Headless One-Shot Run | contract-level | `forge run agent` is a stable CLI entry point. |
| UAT-09 Event Log Durability and Replay | contract-level | On-disk schema invariant. |
| UAT-10 UDS Protocol Error Handling | contract-level | Wire-level robustness. |
| UAT-11 Multi-Client Attach | contract-level | Concurrency contract. |
| UAT-12 CLI Argument Validation | contract-level | Exit codes + usage are part of the CLI contract. |
| UAT-13 Workspace Isolation | contract-level | `.forge/` placement + gitignore behaviour is durable. |

Phase 0 is essentially all `contract-level`: it pre-dates the GUI and tests the CLI / IPC primitives that everything else depends on.

### Phase 1 (`docs/testing/phase1-uat.md`)

| UAT | Tentative label | Rationale |
|-----|-----------------|-----------|
| UAT-01a Outcome gate (MockProvider) | acceptance-only | Specific Dashboard + Session-window + ChatPane flow is Phase 1 UI. |
| UAT-02 Dashboard sessions list | acceptance-only | Dashboard layout changes in Phase 3. |
| UAT-04 Session window lifecycle | acceptance-only | Single-window model is Phase 1 specific. |
| UAT-05 Chat pane streaming and composer | acceptance-only | Specific ChatPane component contract. |
| UAT-06 Tool call card rendering | acceptance-only | Specific card component. |
| UAT-07 Four-scope inline approval | acceptance-only | Specific inline-prompt UI. (The four-scope semantics are durable and live in Phase 0 UAT-06's coverage.) |
| UAT-08 fs.write / fs.edit through GUI | contract-level | `forge-fs` `allowed_paths` enforcement is durable. |
| UAT-09 Persist session archive on end | contract-level | Disk layout for archived sessions is durable. |
| UAT-10 Ephemeral session purge on end | contract-level | Cleanup invariant. |
| UAT-11 Multi-session isolation | contract-level | Cross-session whitelist isolation is durable. |
| UAT-12 Recovery — provider/daemon disappears | acceptance-only | Specific UI surfacing. (Backend resilience is unit-tested.) |
| UAT-13 CLI / GUI parity spot check | contract-level | "No GUI-only features" invariant per `AGENTS.md`. |
| UAT-14 Webview CSP enforcement | contract-level | Production CSP directive set is a durable security contract. |
| UAT-15 ApprovalPrompt accessibility | acceptance-only | Specific component a11y wiring. |

### Phase 2 (`docs/testing/phase2-uat.md`)

| UAT | Tentative label | Rationale |
|-----|-----------------|-----------|
| UAT-01 Four-pane layout, drag-to-dock, persistence | acceptance-only | Specific drag-to-dock UI flow. (Layout-persistence on-disk format could move to a contract-level UAT in a follow-up.) |
| UAT-02 EditorPane Monaco + LSP | acceptance-only | Specific editor component flow. |
| UAT-03 TerminalPane rendering | acceptance-only | Specific terminal component flow. |
| UAT-04 MCP import + tool invocation | contract-level | `.mcp.json` schema, `forge mcp import` CLI, transport behaviour all durable. |
| UAT-05 Agents + sub-agents + AGENTS.md | contract-level | `.agents/*.md`, AGENTS.md auto-injection + 256 KiB cap are durable contracts. |
| UAT-06 Background agents + Agent Monitor | acceptance-only | Specific monitor UI. (Background-agent lifecycle IPC is implicitly covered by UAT-04 / UAT-05.) |
| UAT-07 @-context picker | acceptance-only | Specific composer component. |
| UAT-08 Re-run Replace + Branch scaffolded | acceptance-only | Specific message-actions UI. |
| UAT-09 Loading / empty / error state coverage | acceptance-only | Specific component-state surface. |
| UAT-10 Security gates (SSRF, sandbox-escape, cross-session authz) | contract-level | Security invariants are durable by definition. |
| UAT-11 Command palette | acceptance-only | Specific Cmd/Ctrl+Shift+P UI. |
| UAT-12 Persistent settings + persistent approvals | contract-level | On-disk `settings.toml` / `approvals.toml` schemas are durable. |

---

## When the convention does not apply

- **Backend primitives covered by unit tests, not UATs.** Every phase plan has a "covered by unit tests" appendix. Those entries are not UAT scenarios and need no label — they are already enforced at the crate boundary.
- **Audit-children covered by their respective audit reports.** Same reasoning: those are tracked through audit-report DoD, not UATs.

---

## Authoring checklist

Before opening a milestone UAT PR:

- [ ] Every `## UAT-NN:` heading in the new plan has a `**Type:**` line as its first metadata line.
- [ ] The value is `contract-level` or `acceptance-only` (no other string).
- [ ] No scenario is dual-labelled. If one would have been, it was split into `UAT-NNa` / `UAT-NNb`.
- [ ] Ambiguous classifications are called out in the PR description for reviewer attention.
- [ ] If the plan touches a durable surface that the prior phase already tested, the prior plan's `contract-level` UAT is referenced rather than duplicated.

> **Post-Phase-3.1 note.** Findings M4, M5, H4, and H5 in this audit are all scoped to `crates/forge-providers/src/ollama.rs` (the dedicated `OllamaProvider` NDJSON decoder and URL validation). That crate was removed in Phase 3.1 in favour of a `custom_openai` preset, so those four findings are superseded by the removal itself. The original write-ups are preserved as-is for the historical record.

## Summary

**Milestone:** Phase 1: Single Provider + GUI
**Baseline:** git tag `phase-0` (`a5d498bee`) → HEAD (`b3ba56ec0d` at audit time)
**Areas audited:** 9 (7 Rust crates + `web/packages/app` + `web/packages/ipc`)
**Findings by severity:** **critical 0 / high 11 / medium 12 / low 6** (supply-chain hygiene filed as one bundled `low` issue)

Scope was **not a diff** — it was the current state of every crate and top-level area touched by any Phase-1 PR, plus adjacent code owned by the same crates. Audit raw outputs, including scanner JSON and agent finding dumps, are preserved at `/tmp/forge-audit-phase-1/`.

The audit produced a specific verdict: **Phase 1's trust boundaries are present but unenforced** — the four-scope approval UI exists but its scope is silently collapsed to `Once`; `allowed_paths` is literally the glob `"**"`; the UDS socket mode is world-readable in the fallback case; per-session authorization in the Tauri command layer does not exist; CSP is `null`. These aren't missing features — they are shipped-with-defaults-that-weren't-tightened.

Notably, the single attack class that would have been **critical** — shell-command injection in `shell_exec` — was **not found**. The tool uses `Command::arg(...)` style, not `sh -c $input`. That is the biggest risk *ruled out* by this audit.

## Threat model

Derived in Step 2 from the milestone's newly-introduced capabilities (Ollama streaming, Tauri webview, four-scope approval, forge-fs read/write/edit, Level-1 sandbox, session archive, shell_exec).

**High (LLM → side-effect boundaries introduced in Phase 1):**
- **T1: Tool-approval bypass** — approval UI gates execution; race or wrong-scope dispatch
- **T2: Path traversal / `allowed_paths` bypass** — forge-fs accepts LLM paths; canonicalization/symlink gaps
- **T3: Shell command injection in `shell_exec`** — sh -c vs Command::arg (severity ceiling: critical if present)
- **T4: Sandbox escape** — 6 unsafe libc blocks; signal races; kill-on-drop; macOS/Windows deferred
- **T5: XSS / raw-HTML injection** — CSP=null; inner-HTML sinks on LLM/tool output

**Medium:**
- **T6: UDS socket mode not 0600**
- **T7: IPC deserialization attacks** — serde untagged enums, oversized/nested JSON, type confusion at Tauri boundary
- **T8: NDJSON stream parsing DoS** — unterminated/oversized lines from Ollama
- **T9: Session archive disclosure/integrity**
- **T10: Production panic on attacker-reachable path**

**Low / supply-chain:**
- **T11: Supply-chain new-dep risk** (handled by scanners)
- **T12a: Raw-PID signal input validation** — sign check, pid-file contents, session-id format validation before `libc::kill` / path construction
- **T12b: Raw-PID reuse / signal-to-wrong-process** — TOCTOU between pid-file read and signal; mitigated by `pidfd_open` (Linux) or start-time verification

**Explicitly out of scope for Phase 1:** credential storage / auth (Phase 3), TLS cert validation (localhost HTTP only), multi-tenant / authorization (single-user desktop), container-level isolation (Phase 3 is Level 2+).

## Scope

| Area | Purpose | Phase-1 scope note |
|------|---------|--------------------|
| `crates/forge-cli` | CLI harness (`forge` bin + `forge-cli` lib); spawns/attaches to `forged`; signal-based kill | ~3 files touched |
| `crates/forge-core` | Shared types + wire-format schemas (auto-exported to TS via `ts-rs`) | ~3 files touched |
| `crates/forge-fs` | Pure-Rust file read/write/edit with path validation, glob matching, atomic rename | ~5 files touched; new crate this milestone |
| `crates/forge-providers` | Provider trait + `OllamaProvider` (NDJSON streaming to `http://127.0.0.1:11434`) | ~4 files touched |
| `crates/forge-session` | Session daemon (`forged`); approval gate, tool dispatcher, sandbox L1, UDS server, archive | ~16 files touched (largest surface) |
| `crates/forge-shell` | Tauri 2 desktop host; webview; bridge to `forged` UDS | ~18 files touched |
| `web/packages/app` | Solid.js frontend; dashboard + session window + four-scope approval UI | ~70 files touched |
| `web/packages/ipc` | TS type bindings (auto-generated + small hand-written index) | no security findings |
| `web/packages/design` | Design tokens (pure CSS) | excluded from audit scope |

PRs merged under the milestone span `#51`, `#58`–`#71`, `#78`–`#83` (20 PRs closing 20 F-numbered issues F-018 through F-041).

## Findings

### HIGH

| ID | Issue | Title | Area | Threat |
|----|-------|-------|------|--------|
| H3 | #84 (F-042) | `write_preview` leaks arbitrary file contents into approval event | `forge-fs` | T2 |
| H7 | #85 (F-043) | `allowed_paths = ["**"]` — fs tools reach any absolute path | `forge-session` | T1/T2 |
| H8 | #86 (F-044) | UDS socket falls back to world-accessible `/tmp` path with shared UID | `forge-session` | T6 |
| H4 | #87 (F-045) | NDJSON line buffer unbounded — local squatter can OOM session | `forge-providers` | T8 |
| H5 | #88 (F-046) | `reqwest` client has no timeouts — slow-drip DoS | `forge-providers` | T8 |
| H6 | #89 (F-047) | `shell.exec` timeout orphans sandboxed child; survives shutdown | `forge-session` | T4 |
| H1 | #90 (F-048) | `session_kill` passes `pid<=0` to `libc::kill`, signaling process group | `forge-cli` | T12a |
| H2 | #91 (F-049) | `session_kill` has no PID ownership/staleness check | `forge-cli` | T12b |
| H9 | #92 (F-050) | CSP is `null` — no defense-in-depth against webview XSS | `forge-shell` | T5 |
| H10 | #93 (F-051) | Tauri commands have no per-session authorization | `forge-shell` | T1 |
| H11 | #94 (F-052) | `session_hello` accepts arbitrary filesystem `socket_path` | `forge-shell` | T7 |

### MEDIUM

| ID | Issue | Title | Area | Threat |
|----|-------|-------|------|--------|
| M7 | #95 (F-053) | Client-supplied `ApprovalScope` ignored; always records `Once` | `forge-session` | T1 |
| M8 | #96 (F-054) | `shell.exec` `cwd` verbatim and omitted from approval preview | `forge-session` | T1/T3 |
| M9 | #97 (F-055) | Sandbox missing `RLIMIT_NPROC/NOFILE/FSIZE` — fork-bomb/disk-fill | `forge-session` | T4 |
| M6 | #98 (F-056) | UDS pre-bind `remove_file` → bind is a TOCTOU race | `forge-session` | T6 |
| M1 | #99 (F-057) | Unvalidated `session_id` interpolated into pid/socket paths | `forge-cli` | T12a |
| M5 | #100 (F-058) | `OLLAMA_BASE_URL` trusted without scheme/host validation | `forge-providers` | T7 |
| M4 | #101 (F-059) | `list_models()` buffers entire response body | `forge-providers` | T7 |
| M2 | #102 (F-060) | Unbounded line reads in `event_log`/`Transcript` readers | `forge-core` | T7 |
| M3 | #103 (F-061) | No size limit on `read_file`/`write`/`edit` — memory DoS | `forge-fs` | T10 |
| M10 | #104 (F-062) | `session:event` broadcast app-wide — cross-session disclosure | `forge-shell` | T5 |
| M11 | #105 (F-063) | Capability glob `session-*` + unvalidated session-id in label | `forge-shell` | T5 |
| M12 | #106 (F-064) | `session:event` payloads cast to string without narrowing | `web/packages/app` | T7 |

### LOW

| ID | Issue | Title | Area | Threat |
|----|-------|-------|------|--------|
| L1 | #107 (F-065) | `SessionMeta` lacks `deny_unknown_fields` — forward-compat drift | `forge-core` | T1 |
| L2 | #108 (F-066) | `shell.exec` `timeout_ms` has no ceiling | `forge-session` | T3/T4 |
| L3 | #109 (F-067) | `ToolCallId` 64-bit entropy — bump at next touch | `forge-core` | T1 |
| L4 | #110 (F-068) | `session_send_message` no text-size bound below 4 MiB wire cap | `forge-shell` | T7 |
| L5 | #111 (F-069) | `session_approve_tool` accepts `scope: String` (not enum-typed) | `forge-shell` | T7 |
| S | #112 (F-070) | Phase 1 supply-chain hygiene (cargo scanners in CI, licensing, unmaintained deps) | cross-cutting | T11 |

### Informational (documented here, no issue filed)

| # | Note |
|---|------|
| I1 | The pre-audit scope-gathering report claimed `sandbox.rs:410` had a production `panic!`. The agent confirmed the panic is inside `#[cfg(all(test, target_os = "linux"))] mod tests` — it ships only in the test binary, not `forged`. The scope package should be updated accordingly. |
| I2 | `session_hello` bridge registry has a TOCTOU window between `contains_key` and `insert` with a UDS handshake in between. Not exploitable today (the map is per-app, session ids are unique, and overwriting drops the earlier connection cleanly) but worth noting as a source of `already connected` confusion under future refactors. |

## Automated scanners

All scanners were run at the HEAD committed state; raw output is preserved at `/tmp/forge-audit-phase-1/scanners/` for future comparison.

### `cargo audit`

- **0 vulnerabilities**
- 17 unmaintained warnings — `gtk-rs` family (9, all via Tauri 2's webkit2gtk dep chain), `fxhash` (RUSTSEC-2025-0057), `unic-char-*` family (5), `proc-macro-error` (build-time only)
- 2 unsound warnings — RUSTSEC-2024-0429 (glib VariantStrIter iterator impl), RUSTSEC-2026-0097 (`rand` with custom logger)

All warnings are transitive through Tauri; none immediately actionable from this repo. Tracked in the supply-chain hygiene issue.

### `cargo deny`

Required a `deny.toml` and an `advisory-db` fetch; both set up as part of this audit. Configuration left at `/home/jeroche/repos/forge/deny.toml` as a seed file.

- 0 advisories (matches `cargo audit`)
- 17 unmaintained errors (same set as `cargo audit`)
- **12 unlicensed errors — every workspace-member Cargo.toml is missing a `license` field.** Tracked in the supply-chain hygiene issue.
- 1 license-clarify needed for `ring` (OpenSSL-compat license)

### `pnpm audit`

- 2 moderates, both **dev-server only**:
  - `esbuild ≤0.24.2` (GHSA-67mh-4wv8-2f99): "any website can send requests to the dev server"
  - `vite ≤6.4.1` (GHSA-vg6x-rcgg-rjx6): path traversal in optimized-deps `.map` handling

Neither ships in the production webview. Tracked in the supply-chain hygiene issue.

### Scanner integration gap

`cargo audit` and `cargo deny` were not installed locally before this audit and are not wired into CI. This is itself a finding and is tracked in the supply-chain hygiene issue (#112).

## Areas explicitly cleared

- **`crates/forge-core`** is clean on T1 (only daemon-side code writes `SessionMeta`; no webview IPC path), clean on T7 discriminator confusion (`Event` uses internally-tagged enums; no `untagged`), and clean on T10 on production paths.
- **`crates/forge-providers`** is clean on TLS-cert-validation (no non-loopback URL is contacted by default; loopback HTTP is the intended model for Phase 1).
- **`crates/forge-session` shell.exec command construction** — uses `Command::arg(...)` style; not reachable via `sh -c $input`. The would-have-been-critical T3 finding does not exist.
- **`web/packages/app`** raw-HTML posture — zero matches across the tree for any raw-HTML sink, dangerous set-HTML helper, dynamic-code-eval pattern, or markdown renderer. The CSP-null posture (H9) is the exposure today; current code does not exercise it.
- **`web/packages/ipc`** has no hand-written attack surface worth an issue (it is almost entirely auto-generated types).

## Next steps (for the milestone-completion workflow)

This is the Phase-A security pass. The remaining milestone-completion workflow (quality review, frontend review, docs audit, UAT authorship, perf/backcompat regression gates, release-readiness decision) will consume these issues — any open `high`-severity security issue at release-readiness time is a blocker by the milestone-release-readiness skill's policy.

## Errata

- **2026-04-18** — Threat class **T12** was retroactively split into **T12a** (raw-PID signal input validation — sign check, pid-file contents, session-id format) and **T12b** (raw-PID reuse / signal-to-wrong-process — TOCTOU). H1 and M1 are now T12a; H2 is T12b. The single-bucket T12 originally filed was too coarse to distinguish the sign-validation threat (`kill(0)` broadcast) from the PID-reuse race, and that coarseness caused a briefing error during fix orchestration. Individual finding issue bodies retain their original T12 label for historical fidelity; this table and threat model section are the authoritative classification. The split applies retroactively to all Phase 1 findings and is the reference going forward.

## Labels

This issue: `type: security`, `security: audit`.
All finding issues: `type: security`, `security: {critical,high,medium,low}`.


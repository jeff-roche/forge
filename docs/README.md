# Forge Docs

Navigation hub for all Forge documentation. One topic per file; use the table below to find what you need.

---

## Folders

| Folder | What's inside |
|---|---|
| `product/` | Why Forge exists, mental models, AI UX commitments, scope |
| `architecture/` | Tech stack, system structure, IPC contracts, data model |
| `frontend/` | Solid app architecture, state model, Monaco hosting |
| `design/` | Visual identity, color system, typography, components, copy |
| `ui-specs/` | Per-component specs for every Forge-specific surface |
| `build/` | Repo layout, testing strategy, phased roadmap, sprint tickets, risks |

---

## product/

| File | Description |
|---|---|
| [vision.md](product/vision.md) | What Forge is, the mental model inversion, and what success looks like |
| [core-concepts.md](product/core-concepts.md) | The six vocabulary items: Workspace, Session, Provider, Skill, MCP Server, Agent |
| [ai-ux.md](product/ai-ux.md) | Transparency, @-context, multi-provider, memory, background agents, approval |
| [scope.md](product/scope.md) | v1.0 in/out of scope, deliberately deferred items, open questions |

## architecture/

| File | Description |
|---|---|
| [overview.md](architecture/overview.md) | Tech stack rationale, full dependency matrix, configuration conventions |
| [sessions-end-to-end.md](architecture/sessions-end-to-end.md) | Full session lifecycle: + New session click → composer send → streamed token, with code map and error paths |
| [window-hierarchy.md](architecture/window-hierarchy.md) | Dashboard window, Session window, Command palette |
| [session-layout.md](architecture/session-layout.md) | Pane types, layout rules, and full CLI surface for sessions |
| [isolation-model.md](architecture/isolation-model.md) | Three isolation levels, approval model, and sandboxing implementation |
| [crate-architecture.md](architecture/crate-architecture.md) | All 12 crates with full responsibility breakdown, key types, and deps |
| [ipc-contracts.md](architecture/ipc-contracts.md) | Both IPC boundaries: Tauri commands/events and UDS framing/protocol |
| [provider-abstraction.md](architecture/provider-abstraction.md) | Unified chat request, streaming chunks, tool classification |
| [persistence.md](architecture/persistence.md) | Filesystem-only layout, metadata schemas, archive/reactivation |

## frontend/

| File | Description |
|---|---|
| [architecture.md](frontend/architecture.md) | Solid signals, state model, Monaco iframe hosting, streaming |
| [generation-pipelines.md](frontend/generation-pipelines.md) | Design token drift check and ts-rs Rust to TypeScript IPC type generation, both CI-enforced |

> Monaco hosting's authoritative postMessage protocol and LSP lifecycle live in [`web/packages/monaco-host/README.md`](../web/packages/monaco-host/README.md), co-located with `src/protocol.ts`. `frontend/architecture.md §9.3` summarizes and cross-refs it.

## design/

| File | Description |
|---|---|
| [philosophy.md](design/philosophy.md) | Three principles, visual identity, mark construction, wordmark |
| [color-system.md](design/color-system.md) | Ember brand scale, Iron surface scale, semantic colors, token rules |
| [typography.md](design/typography.md) | Font families, type scale, per-family rules, ligatures |
| [spacing-layout.md](design/spacing-layout.md) | Base-4 spacing scale, border radii, shell structure, elevation order |
| [component-principles.md](design/component-principles.md) | Buttons, inputs, toasts, status bar, code blocks |
| [ai-patterns.md](design/ai-patterns.md) | Provider identity, streaming state, tool calls, sub-agent banners, MCP |
| [voice-terminology.md](design/voice-terminology.md) | Copy principles, terminology table, visual and behaviour do/don't |
| [token-reference.md](design/token-reference.md) | Complete CSS custom property definitions at `:root` |

## ui-specs/

| File | Description |
|---|---|
| [shell.md](ui-specs/shell.md) | 44px activity bar, 32px title bar, 22px status bar, window behaviours |
| [layout-panes.md](ui-specs/layout-panes.md) | Single/H/V/grid splits, pane header, drag-to-dock, minimum width |
| [chat-pane.md](ui-specs/chat-pane.md) | Message anatomy, composer structure, streaming behaviours, keyboard |
| [tool-call-card.md](ui-specs/tool-call-card.md) | Inline expandable card, approval gating, parallel grouping |
| [sub-agent-banner.md](ui-specs/sub-agent-banner.md) | Orchestrator spawn visualization, collapsible sub-thread |
| [context-picker.md](ui-specs/context-picker.md) | @-context picker popup, categories, chip insertion |
| [provider-selector.md](ui-specs/provider-selector.md) | Live model/provider switching, per-message selection |
| [agent-monitor.md](ui-specs/agent-monitor.md) | Three-column: agent list, trace timeline, inspector panel |
| [approval-prompt.md](ui-specs/approval-prompt.md) | Inline four-scope whitelisting, keyboard shortcuts, preview by tool type |
| [streaming-states.md](ui-specs/streaming-states.md) | Streaming cursor, pulse ring, transitions, and complete motion reference |
| [session-roster.md](ui-specs/session-roster.md) | *(deferred post-Phase-2)* Scope-aware asset display and the repeatable asset card — no component exists yet |
| [branching.md](ui-specs/branching.md) | Message tree data model, variant rendering, conversation flow |

## build/

| File | Description |
|---|---|
| [approach.md](build/approach.md) | Five core commitments, what not to build first, repo layout rationale |
| [testing.md](build/testing.md) | Unit, integration, smoke test levels, mock provider, coverage targets |
| [roadmap.md](build/roadmap.md) | Phase 0-4 outcomes, first sprint tickets F-000 to F-017 |
| [risks.md](build/risks.md) | Five high-risk items, five medium-risk items, four unresolved unknowns |

# AI Patterns

> Extracted from DESIGN.md §7 — provider identity, streaming state, tool call visibility, sub-agent banners, agent monitor, and MCP indicators

**Related docs:** [Color System](color-system.md) · [Component Principles](component-principles.md) · [Voice & Terminology](voice-terminology.md)

---

## 7. AI-Specific UI Patterns

These patterns are Forge-specific. Follow them consistently — they are part of the product's identity.

### Provider identity

Each AI provider gets an accent color used consistently across all panes (see [Color System](color-system.md) for the named scales and [Token Reference](token-reference.md) for the `--color-provider-*` CSS variables):
- Anthropic / Claude: `ember-400` (`#ff4a12`)
- OpenAI / GPT: `amber` (`#ffaa33`)
- Local / OpenAI-compatible (Ollama, LM Studio, vLLM, …): `steel` (`#7aaaff`)
- Custom endpoints: `iron-200` (`#8a9aac`)

The provider dot, send button, pane header indicator, and agent thread accent all use this color. When a user opens a pane, the color instantly communicates which model is active.

### Streaming state

Streaming responses show a blinking cursor: `width: 5px`, `height: 12px`, `background: ember-400`, `border-radius: 1px`, blinking at 1s intervals (50% duty cycle). This cursor is always present during streaming and disappears the moment streaming ends. It should never appear on a completed message.

### Tool call visibility

Tool calls surface as inline collapsible cards within AI message bubbles:
- Icon: `ember-100` (gold) for tool calls, `steel` for resource reads, `ember-400` for agent spawns
- Background: `rgba(255,209,102,0.04)` with `rgba(255,209,102,0.15)` border
- Always show: tool name, arguments (truncated if long), result, duration
- Always collapsible — expanded by default for the most recent call, collapsed for older ones

### Sub-agent banners

When an orchestrator spawns a sub-agent, it appears as a banner within the parent chat thread:
- Left icon: colored by the spawning model's accent color
- Title prefix: always `↳ spawned sub-agent: [name]`
- Status chips: model name, tool count, running/done/queued/error state
- Sub-thread: indented with a `2px border-left` in `iron-600`, collapsible

### Agent monitor

The agent monitor shows a list of all agents with a progress bar (3px height, `iron-800` track) and a step-by-step trace on selection. Progress bars use semantic colors: green for done, amber for running, iron-500 for queued, ember for error.

### MCP servers

MCP servers in the panel show a 7px status dot with a glow shadow when connected: `box-shadow: 0 0 6px rgba(61,220,132,0.5)`. This glow is the only glow used in the UI — it specifically communicates live network connectivity, which is a meaningfully different state than simple "active".

### AGENTS.md injection

When a workspace contains `AGENTS.md`, its contents are prepended to every agent's system prompt for that session. This lets repository authors supply agent-level instructions (coding style, project context, constraints) that apply automatically.

**Security posture.** `AGENTS.md` is user-controlled content from a potentially untrusted source. The backend enforces two hard guards:

- **Size cap (256 KiB).** Files larger than `AGENTS_MD_SIZE_CAP` (256 KiB) are refused with `Error::AgentsMdTooLarge` and treated as absent. The session continues normally; a `warn!` is emitted in the daemon log.
- **Injection warning.** Every time `AGENTS.md` is successfully injected, the daemon logs a `warn!` naming the file path. This makes injection visible in traces and audit logs.

**What is deferred.** Trust-on-first-use (hash-on-first-use accept/reject dialog, per-workspace trust registry) is not yet implemented. Until it is, users opening repositories from untrusted sources should review `AGENTS.md` manually before starting a session. A follow-up issue (linked from F-352) tracks the full trust-on-first-use gate.

### Interaction states

Every component that fetches, renders, or mutates data resolves into one of four states. Use the shapes below — do not invent alternatives. New components that omit a state must justify the omission in review.

- **loading:** skeleton placeholder matching the final layout, or the streaming cursor (see §"Streaming state"). Never a spinner.
- **empty:** `iron-300` copy in a noun-phrase form describing what would appear here, plus an optional primary action (see [Voice & Terminology](voice-terminology.md) §8 "Empty-state copy" for the canonical phrasing). No illustration.
- **error:** `ember-400` border on the affected surface plus a persistent toast (see [Component Principles](component-principles.md) §"Toasts"). The toast carries the technical identifier verbatim; the border marks the scope of the failure.
- **success:** implicit. No banner, no toast, no checkmark unless the action is dismissable (e.g. a confirmation the user must acknowledge before the surface returns to its default state).

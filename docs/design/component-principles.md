# Component Principles

> Extracted from DESIGN.md §6 — buttons, inputs, toasts, status bar, and code blocks

**Related docs:** [Color System](color-system.md) · [Token Reference](token-reference.md) · [Typography](typography.md)

---

## 6. Component Principles

### Buttons

Four variants: **primary**, **secondary**, **ghost**, **icon**. Color names below (`ember-400`, `iron-600`, etc.) are defined in the [Color System](color-system.md) and exposed as CSS custom properties in [Token Reference](token-reference.md).

- **Primary** (`ember-400` fill): one per view maximum. The most important action.
- **Secondary** (ember outline): alternative actions, same importance as primary but not the default.
- **Ghost** (neutral border): destructive alternatives, cancel, skip.
- **Icon** (neutral border): toolbar actions, panel controls.

All buttons use Barlow Condensed 700, uppercase, `letter-spacing: 0.1em`. Active state always includes `transform: translateY(1px)`.

Disabled buttons use `iron-600` background and text. Never reduce opacity on a button to show disabled state — opacity makes elements appear interactive.

### Inputs

- Default border: `iron-600`
- Hover border: `iron-300`
- Focus border: `ember-400` — always, without exception
- Error border: `ember-400` (same as focus — the context makes the meaning clear)
- Success border: `green` (`#3ddc84`)

Labels use `mono-xs` style: Fira Code, 9px, uppercase, `letter-spacing: 0.2em`, `iron-300` color.

### Toasts

Toasts have a 3px left accent bar (the semantic color), a dark tinted background, and a semantic border. They stack from the bottom-right, above the status bar, maximum 4 visible.

- Success: auto-dismiss 5s
- Info: auto-dismiss 5s
- Warning: auto-dismiss 8s
- Error: persists until actioned

### Status bar

The status bar is always `ember-400` background with white text. This is the most visible brand surface in daily use. It always shows: the Forge mark, active provider, streaming state, and file context. Do not change the status bar color under any circumstances, including in light mode.

The ember-400 × white pairing computes to ~3.35:1 contrast — below WCAG AA 4.5:1 for normal text. This is an accepted brand exception documented in [Color System §Brand exception — status bar](color-system.md#brand-exception--status-bar) and pinned by `web/packages/app/src/shell/StatusBar.css.test.ts`. The exception covers only the bar body; interactive controls rendered on the bar (e.g. the background-agents badge) must render on a solid iron chip so they clear WCAG AA 4.5:1 independently of the ember background.

### Code blocks

Code blocks use `#050709` background (slightly darker than `iron-900`) to create depth. The header bar shows language and copy/insert actions. Highlighted lines use a left border of `ember-400` with `rgba(255,74,18,0.07)` background.

### Layout primitives

V1 dashboard primitives are specified in [DESIGN.md §Layout primitives](../../DESIGN.md#layout-primitives). Each primitive binds to tokens in [Token Reference](token-reference.md); component CSS resolves to those tokens via `var(--token)`.

- **Layout grid** — 12-column track, `var(--sp-4)` gap, `var(--sp-6) var(--sp-8)` outer padding. Spans: `col-4`, `col-6`, `col-8`, `col-12`.
- **Hero block** — headline + status sentence + dual CTAs. Ghost secondary leads, ember primary trails.
- **KPI tile** — `surface-2` card with a 2px top-left accent rail. Variants bind to `ember-400` / `success` / `warning` / `info`.
- **Spark chart** — 60px micro-trend on `ember-400` line + alpha-gradient fill. Latest day-dot is haloed `1px` white.
- **Toggle switch** — 28×16 track on `border-1` / `ember-900`, 12px thumb on `text-tertiary` / `ember-400`.
- **Status pill** — `mono-xxs` label + 6px pulse dot. Variants: `streaming` (ember-200), `awaiting approval` (warning), `done` (success), `idle` (text-secondary), `ready` (success + glow), `auth` (error + glow).
- **Status bar** — 22px Ember 400 strip with left/right slots, mono-xxs text, `·` separators. The brand-exception surface — never re-colored.

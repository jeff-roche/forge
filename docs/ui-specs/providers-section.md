# Providers Section

> Dashboard section ([F-586](https://github.com/forge-ide/forge/issues/604)) — one card per built-in or user-configured provider; clicking flips `[providers.active]` and the change propagates to live sessions.

---

## Purpose

Confirm which providers are reachable, surface model + credential gaps at a glance, and let the user switch the workspace's active provider with one click. The active selection is the one any new session inherits and — via the `provider:changed` Tauri event — the one any open session swaps to on its next turn.

## Where

`<ProvidersSection>` mounts inside the Dashboard root, between the title and the Credentials section. Component path: `web/packages/app/src/components/dashboard/ProvidersSection.tsx`.

## Size

Fills the dashboard column width. The grid uses `auto-fill, minmax(220px, 1fr)`, so cards re-flow from a single column on narrow widths up to four-across at typical desktop widths. No fixed height — the section grows with the provider count.

## Structure

```
┌─ PROVIDERS ────────────────────────────────────────────────────┐
│ ┌──────────────┐ ┌──────────────┐ ┌──────────────────┐         │
│ │ Anthropic  ●│ │ OpenAI       │ │ custom · ollama  │         │ ← cards
│ │ sonnet-4.5  │ │ no model     │ │ llama3.2         │         │
│ │ ✓ key       │ │ ⚠ key        │ │                  │         │
│ └──────────────┘ └──────────────┘ └──────────────────┘         │
└────────────────────────────────────────────────────────────────┘
```

Local model servers (Ollama, LM Studio, vLLM, …) appear here as user-added `custom_openai:<name>` entries — the Add Provider modal on the Providers page ships a "local Ollama" preset that auto-fills the local endpoint, a default model, and keyless auth. There is no built-in `ollama` card.

The grid is a `Tabs` component in `radio` variant — the cards are a single-select radiogroup with proper ARIA. A roving-tabindex (`useRovingTabindex`) keeps the section a single Tab stop with arrow-key navigation between cards.

### Card anatomy

- **Top row:** `display_name` (left) and an active pip (right, ember dot) when this card is the active provider.
- **Bottom row (meta):** model hint + credential hint, separated visually.
  - **Model hint:** the configured model id when one is reachable, or `no model` (text-tertiary) when `model_available === false`.
  - **Credential hint:** `✓ key` (color-ok) when stored, `⚠ key` (color-warn) when missing. Only rendered for providers that set `credential_required = true` — keyless `custom_openai` presets (e.g. the local-Ollama preset) omit this row entirely so the chrome stays honest.
- **Pending state:** while `set_active_provider` is in flight, the card sets `aria-busy=true` and adopts the `provider-card--pending` modifier so the user sees a single locked-in transition rather than a flicker.

## States

The section renders all four `docs/design/component-principles.md` states distinctly — a `list_providers` rejection must never collapse into the empty placeholder.

- **Loading.** Skeleton placeholder per `dashboard.md §D.5.1` — four `card`-variant skeletons on the live grid. The `<Skeleton>` primitive carries `role="status"` + `aria-busy="true"` + `aria-live="polite"`.
- **Error.** Visible block inside the section with heading `PROVIDERS UNAVAILABLE`, the verbatim error detail (preserved per `voice-terminology.md` §8 "show technical identifiers verbatim"), and the active selection left untouched.
- **Empty.** Not reachable in practice — Phase 3 always ships at least the built-in providers — but if `list_providers` returns zero entries, the grid renders nothing (no placeholder text); the calling Dashboard treats absence as a configuration bug, not a UI concern.
- **Ready.** The card grid above.

A separate `ACTION ERROR` line surfaces under the grid when `set_active_provider` itself rejects, distinct from the load error so the user can tell "I can't read providers" from "I tried to switch and it failed".

## Copy

- Section label: `PROVIDERS`
- Loading skeleton: no copy (skeleton chrome only — see `dashboard.md §D.5.1`)
- Load error heading: `PROVIDERS UNAVAILABLE`
- Action-error prefix: `set_active_provider failed: <detail>`
- Model-missing hint: `no model`
- Credential indicators: `✓ key` (stored) / `⚠ key` (missing)
- Card aria-label: `Select <display_name>[, credential missing][, no model configured][, switching]` — composed in declaration order so screen-reader output stays predictable.

## Color & typography

- Active pip: `--color-ember-400` (per `docs/design/ai-patterns.md` provider-accent treatment).
- `✓ key`: `--color-ok` text. `⚠ key`: `--color-warn` text. `no model`: `--color-text-tertiary`.
- Card border: 1px `--color-border-1`; `--color-ember-400` when selected; `--color-border-2` on hover. All values come from `web/packages/design/src/tokens.css`.
- Display font: `--font-display` for `display_name`, `--font-body` for hints, `--font-mono` for the model id.

## Keyboard

- Tab — moves focus to the section as a single stop (roving-tabindex).
- Arrow Left/Right — moves focus between cards in the radiogroup.
- Space / Enter on a focused card — invokes `set_active_provider`. While the IPC is in flight, all cards report `aria-busy=true` via the grid's `aria-busy` attribute and ignore additional clicks until the call resolves.

## Interactions with other surfaces

- A successful `set_active_provider` IPC emits the `provider:changed` Tauri event app-wide. Live sessions hear the event in their orchestrator and call `SwappableProvider::swap` for the next turn ([F-640](https://github.com/forge-ide/forge/issues/640)). The Providers section itself only refetches its own snapshot; coordinating other windows is the backend's job.
- Selecting a provider whose `credential_required` is true while no credential is stored is *allowed* — the section does not block the click. The first-run `<CredentialBanner>` surfaces above the section to guide the user to the Credentials section ([credentials-section.md](./credentials-section.md)).

## Cross-spec references

- [`dashboard.md`](./dashboard.md) — root-window layout; provider chrome on session cards uses the same accent palette.
- [`dashboard.md §D.5.1`](./dashboard.md#d51-loading-state-primitives-f-684) — skeleton primitive contract (F-684).
- [`provider-selector.md`](./provider-selector.md) — composer-time provider switching. The Dashboard section is the *workspace-level* selector; the composer selector is per-turn.
- [`credentials-section.md`](./credentials-section.md) — neighbouring section that owns key storage; the providers grid only surfaces `has_credential` as a hint.
- `docs/design/ai-patterns.md` — provider accent colors used by the active pip and any future provider chip.

## Doesn't do

- Does not configure providers — provider definitions live in `~/.config/forge/providers.toml`; the section only reflects status.
- Does not store credentials — see `credentials-section.md`. Clicking a card with a missing key still flips active; sessions surface the "key missing" error at start time.
- Does not show usage / cost — see [`usage.md`](./usage.md).
- Does not let the user *delete* a custom-OpenAI provider entry inline — that's a config-file edit today.

# Pane Header

> Extracted from SPECS.md §3.3 and the F-024 Session-window shell — the 28px header strip that titles every pane and exposes pane-local actions.

---

## PH. Pane header

**Purpose.** Identify the pane (type + subject), surface live cost for chat panes, and expose pane-local actions (close, future overflow). Always 28px tall, always present at the top of every pane.

**Where.** Top edge of every pane in the Session window. One per pane.

**Size.** Height `28px`, width fills the pane.

### PH.1 Structure

```
┌─────────────────────────────────────────────────────────────┐
│ CHAT  refactor session  [● anthropic]   in 1k · out 2k · $0.04   ⋯   CLOSE SESSION │
└─────────────────────────────────────────────────────────────┘
  └type └subject          └provider pill └cost meter           └overflow └close
```

Flex row, vertically centered, `gap: var(--sp-3)`, padding `0 var(--sp-4)`. Background `--color-surface-2`, 1px bottom border `--color-border-1`.

### PH.2 Slots (left to right)

| Slot | Content | Style |
|---|---|---|
| Type label | `CHAT`, `TERMINAL`, `EDITOR` | `--font-mono` 10px, uppercase, letter-spacing 0.12em, color `--color-text-tertiary` |
| Subject | Agent name / filename / shell name | `--font-body` 13px, color `--color-text-primary` |
| Provider pill | `● <provider>` | `--font-mono` 11px, `--color-surface-3` bg, 1px `--color-border-1`, radius `--r-sm`, padding `0 var(--sp-2)`, gap `var(--sp-1)` for the dot |
| Cost meter | `in <n> · out <n> · $<cost>` | `--font-mono` 10px, letter-spacing 0.04em, color `--color-text-secondary`, `margin-left: auto` (right-pushes everything after the subject) |
| Overflow | `⋯` button | placeholder in Phase 1; opens pane-local actions menu (rename, duplicate, move-to-window, etc.) when implemented |
| Close | `CLOSE SESSION` (chat) | `--font-mono` 10px, uppercase, letter-spacing 0.12em, color `--color-text-secondary`; hover bumps to `--color-text-primary` and a 1px `--color-border-2` border with radius `--r-sm` |

### PH.3 Provider pill

The pill is the per-pane provider indicator. Color follows `ai-patterns.md` — anthropic = `ember-400`, openai = `amber`, local / OpenAI-compatible endpoints = `steel`, other custom = `iron-200`. The accent applies to the dot and the pill text. The pill picks the accent from the active session's provider id at render time — `custom_openai:<name>` entries (including local model servers like Ollama via the local preset) resolve to `steel` when the endpoint is loopback and `iron-200` otherwise.

### PH.4 Cost meter format

Format: `in <input-tokens> · out <output-tokens> · $<dollar-cost>`. Tokens are abbreviated (`1.2k`, `34k`); the dollar value uses up to two decimals with no leading zero stripping (`$0.04`, not `$.04`).

The cost meter sits at `margin-left: auto`, so the subject and provider pill cluster at the left and the cost + actions cluster at the right.

**Narrow-width collapse.** Per `chat-pane.md §4` (Narrow-width adaptations), when the pane is below 480px wide, the cost meter collapses to `$<cost>` only and the per-token breakdown moves into a tooltip on hover. Below the 320px pane minimum (per `layout-panes.md §3.7`), the type label and subject also collapse — type label becomes an icon, subject truncates with ellipsis.

### PH.5 Overflow `⋯` menu

Placeholder slot to the left of the close button, holding pane-local actions that don't fit the header inline:

- Rename pane subject
- Duplicate pane (clones into a new split)
- Move to window (detach into a fresh window — Phase 2+)
- Pane-specific: `compact context` for chat panes at high context fill (see `chat-pane.md §4.1` Compact button)

In Phase 1 the slot is reserved but not rendered. When implemented, it opens a small popover anchored to the `⋯` glyph; clicks outside dismiss.

### PH.6 Close button

Text: `CLOSE SESSION` for chat panes (per `voice-terminology.md §8` — verb + noun in display caps; tracked via F-084). The text varies by pane type once non-chat panes ship: `CLOSE TAB` for editor panes that hold a single tab, `CLOSE PANE` for terminal and editor panes generally.

Behavior: clicking invokes the parent's `onClose` callback. For chat panes in the Session window, this resolves to the session-window-close path that confirms when the underlying session has unsaved scrollback.

Accessibility: the rendered text is the convention; `aria-label="Close session window"` carries the screen-reader copy and stays in plain sentence case.

### PH.6a Trailing slot (badges)

An optional per-pane badge slot sits between the subject and the cost meter. Pane types plug it with pane-specific state indicators — today the editor's dirty-dot (6px circle, `--color-accent-warn`) for unsaved buffers; terminals and the chat pane leave it empty.

- **Placement.** Between subject and the `margin-left: auto` cost meter. Document order is `type → subject → trailing → provider → cost → close`, so screen readers traverse pane identity before pane state.
- **Contents.** Caller-supplied JSX. The primitive does not style the badge itself — pane consumers own the visual so each pane's state language (dirty, error, muted, etc.) stays pane-scoped.
- **Narrow-width collapse.** The slot is non-essential chrome: removed from the tree entirely at `icon-only` (<240px) alongside the provider pill and cost meter. It remains at `compact` so the dirty-dot stays visible when only the label collapses.
- **Accessibility.** Badges with meaning carry their own accessible name (`aria-label="unsaved changes"` on the dirty-dot). The trailing `<span>` wrapper has no role so the badge's own semantics surface directly.
- **Not.** Not a drop target for arbitrary actions — action buttons belong in the overflow `⋯` menu (§PH.5). Not a chat-only surface — any pane type can pass a badge.

### PH.6b Landmark role

The pane header's `<header>` element is **sub-structural** (nested inside the pane's `<section>`) and therefore must not carry `role="banner"`. ARIA reserves the banner landmark for the single top-level page banner; a per-pane banner creates duplicate landmarks that confuse AT users. Consumers that need a drag handle on the header pass `onHeaderPointerDown`; the primitive forwards it to the `<header>` element so consumers don't wrap the primitive in an extra `<div>`.

### PH.7 Cross-spec references

- `layout-panes.md §3.3` — defines the 28px slot; this spec details the contents.
- `chat-pane.md §4` — pane header slot in chat-mode (cost meter, narrow-width rules).
- `ai-patterns.md` — provider accent color rules used by the pill.
- `voice-terminology.md §8` — display-caps verb-noun rule for the close button.

**Doesn't do.**
- Does not show usage history — only the running totals for the current pane.
- Does not host pane-switching tabs — those live in the tab bar above the pane (see `layout-panes.md`).
- Does not gate the close action behind a confirm dialog inside the header itself; confirmation, when needed, is owned by the close handler.

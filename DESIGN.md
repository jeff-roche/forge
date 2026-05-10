---
version: alpha
name: Forge
description: >-
  Forge is a native desktop workshop for agentic work. The visual system is
  developer-tool industrial: dark, dense, warm, with one ember accent for
  brand and active state. This document is the source of truth for the
  visual identity and is mirrored by web/packages/design/src/tokens.css.
colors:
  primary: "#ff4a12"
  secondary: "#ffaa33"
  tertiary: "#7aaaff"
  neutral: "#07080a"
  surface: "#0d0f13"
  on-surface: "#eae6de"
  error: "#ff4a12"
  success: "#3ddc84"
  warning: "#ffaa33"
  info: "#7aaaff"
  ember-50: "#fff4d6"
  ember-100: "#ffd166"
  ember-200: "#ffaa33"
  ember-300: "#ff7a30"
  ember-400: "#ff4a12"
  ember-500: "#cc3a00"
  ember-600: "#a32e00"
  ember-900: "#2a0800"
  iron-900: "#07080a"
  iron-850: "#0d0f13"
  iron-800: "#13161d"
  iron-750: "#181c26"
  iron-700: "#1c2230"
  iron-600: "#252f3e"
  iron-500: "#3a4558"
  iron-300: "#8a9aac"
  iron-200: "#8a9aac"
  iron-100: "#eae6de"
  text-primary: "#eae6de"
  text-secondary: "#8a9aac"
  text-tertiary: "#3a4558"
  text-disabled: "#252f3e"
  text-inverted: "#ffffff"
  text-link: "#7aaaff"
  border-default: "#1c2230"
  border-strong: "#252f3e"
  syntax-keyword: "#ff7a30"
  syntax-function: "#ffd166"
  syntax-string: "#3ddc84"
  syntax-type: "#7a9fff"
  syntax-number: "#ff9966"
  syntax-comment: "#3a4558"
  provider-anthropic: "#ff4a12"
  provider-openai: "#ffaa33"
  provider-local: "#7aaaff"
  provider-custom: "#8a9aac"
typography:
  display-2xl:
    fontFamily: Barlow Condensed
    fontSize: 72px
    fontWeight: 900
    lineHeight: 1.0
    letterSpacing: 0.02em
  display-xl:
    fontFamily: Barlow Condensed
    fontSize: 48px
    fontWeight: 800
    lineHeight: 1.05
    letterSpacing: 0.02em
  display-lg:
    fontFamily: Barlow Condensed
    fontSize: 32px
    fontWeight: 700
    lineHeight: 1.1
    letterSpacing: 0.04em
  display-md:
    fontFamily: Barlow Condensed
    fontSize: 22px
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: 0.04em
  body-lg:
    fontFamily: Barlow
    fontSize: 16px
    fontWeight: 400
    lineHeight: 1.5
  body-md:
    fontFamily: Barlow
    fontSize: 14px
    fontWeight: 400
    lineHeight: 1.5
  body-sm:
    fontFamily: Barlow
    fontSize: 12px
    fontWeight: 400
    lineHeight: 1.4
  mono-md:
    fontFamily: Fira Code
    fontSize: 13px
    fontWeight: 400
    lineHeight: 1.5
    fontFeature: '"liga" 1, "calt" 1'
  mono-sm:
    fontFamily: Fira Code
    fontSize: 11px
    fontWeight: 400
    lineHeight: 1.4
    fontFeature: '"liga" 1, "calt" 1'
  mono-xs:
    fontFamily: Fira Code
    fontSize: 9px
    fontWeight: 400
    lineHeight: 1.2
    letterSpacing: 0.3em
    fontFeature: '"liga" 1, "calt" 1'
  button-label:
    fontFamily: Barlow Condensed
    fontSize: 11px
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: 0.1em
  input-label:
    fontFamily: Fira Code
    fontSize: 9px
    fontWeight: 400
    lineHeight: 1.2
    letterSpacing: 0.2em
spacing:
  xs: 4px
  sm: 8px
  md: 12px
  lg: 16px
  xl: 20px
  2xl: 24px
  3xl: 32px
  4xl: 40px
  5xl: 48px
  pane-header-height: 28px
  status-bar-height: 22px
  title-bar-height: 32px
  tab-bar-height: 33px
  activity-bar-width: 44px
  sidebar-width: 190px
rounded:
  sm: 3px
  md: 5px
  lg: 8px
  pill: 10px
  full: 9999px
components:
  button-primary:
    backgroundColor: "{colors.ember-400}"
    textColor: "{colors.text-inverted}"
    rounded: "{rounded.sm}"
    padding: 12px
    typography: "{typography.button-label}"
  button-primary-hover:
    backgroundColor: "{colors.ember-500}"
    textColor: "{colors.text-inverted}"
  button-primary-active:
    backgroundColor: "{colors.ember-600}"
    textColor: "{colors.text-inverted}"
  button-primary-disabled:
    backgroundColor: "{colors.iron-700}"
    textColor: "{colors.text-secondary}"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "{colors.text-secondary}"
    rounded: "{rounded.sm}"
    padding: 12px
    typography: "{typography.button-label}"
  button-ghost-hover:
    textColor: "{colors.text-primary}"
  button-ghost-active:
    backgroundColor: "{colors.iron-750}"
    textColor: "{colors.text-primary}"
  button-danger:
    backgroundColor: "transparent"
    textColor: "{colors.error}"
    rounded: "{rounded.sm}"
    padding: 12px
    typography: "{typography.button-label}"
  button-icon:
    backgroundColor: "transparent"
    textColor: "{colors.text-secondary}"
    rounded: "{rounded.sm}"
    size: 24px
  button-icon-hover:
    textColor: "{colors.text-primary}"
  button-icon-pressed:
    backgroundColor: "{colors.iron-750}"
    textColor: "{colors.text-primary}"
  tab:
    backgroundColor: "transparent"
    textColor: "{colors.text-secondary}"
    rounded: "{rounded.sm}"
    padding: 12px
    typography: "{typography.button-label}"
  tab-selected:
    backgroundColor: "{colors.iron-750}"
    textColor: "{colors.ember-300}"
  chip:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text-secondary}"
    rounded: "{rounded.sm}"
    padding: 8px
    typography: "{typography.mono-sm}"
  chip-context:
    backgroundColor: "{colors.iron-750}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.sm}"
    padding: 8px
    typography: "{typography.mono-sm}"
  input:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.sm}"
    padding: 12px
    typography: "{typography.mono-md}"
  input-focus:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text-primary}"
  card:
    backgroundColor: "{colors.iron-800}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.lg}"
    padding: 20px
  toast:
    backgroundColor: "{colors.iron-800}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.md}"
    padding: 16px
  status-bar:
    backgroundColor: "{colors.ember-400}"
    textColor: "{colors.text-inverted}"
    height: 22px
    padding: 8px
  pane-header:
    backgroundColor: "{colors.iron-800}"
    textColor: "{colors.text-primary}"
    height: 28px
    padding: 16px
  command-palette:
    backgroundColor: "{colors.iron-800}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.lg}"
    padding: 0px
  tooltip:
    backgroundColor: "{colors.iron-800}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.sm}"
    padding: 8px
    typography: "{typography.mono-sm}"
---

# Forge Design System

## Overview

Forge is a native desktop workshop for agentic work — Any AI, one editor, transparent by default. The visual system is **industrial utility**: dense, precise, dark, functional. It is a tool, not a product.

The aesthetic borrows from the terminal and the editor: warm dark surfaces, monospaced metadata, a single accent color for action and identity. There is no decorative ornament. There are no rounded corners on load-bearing UI. There are no gratuitous gradients or empty space. Hierarchy is conveyed by surface elevation and contrast, not by drop shadows or rounded chrome.

The brand color, **Ember (#ff4a12)**, is reserved for the things that matter: the primary action on a screen, the active selection indicator, the streaming cursor, the status bar, and the brand mark itself. Everywhere else, the Iron scale carries the surface and the text — a warm, near-black foundation that sits comfortably under long sessions of focused work.

The audience is developers running serious work through AI providers. The emotional register is **confident and terse** — the UI never apologizes, never explains itself, never asks the user to wait without showing exactly what it is doing. Every AI action surfaces in the chat as a tool-call card; every connection state is visible in the sidebar; every running agent reports its progress in the status bar.

## Colors

The palette is built on two scales — **Ember** for brand and action, **Iron** for surface and text — plus a small set of semantic tokens for state and a single accent blue (Steel) for links and the local-provider identity.

- **Primary — Ember 400 (#ff4a12):** The brand color. Used for the primary action per view, the active/selected indicator, the streaming cursor, error borders, and the status bar background. It is the only color in the system that signals "this is what matters right now."
- **Secondary — Amber (#ffaa33):** Ember 200. Used for the OpenAI provider accent, dirty-buffer indicators in adjacent forms, gradient ends, and warning state.
- **Tertiary — Steel (#7aaaff):** The single blue in the system. Used for links, info state, and the local/Ollama provider accent. No other blues are introduced.
- **Neutral — Iron 900 (#07080a):** The deepest surface. App background. Every other surface in the system stacks lighter than this, in strict order, never inverted.
- **Surface — Iron 850 (#0d0f13):** Panels and sidebars. The default content surface that sits one level above the app background.
- **On-Surface — Cream (#eae6de):** Primary text. Intentionally warm off-white, never pure `#ffffff`. Pure white on a near-black background causes eye strain over long sessions; the cream tone is locked.
- **Success (#3ddc84):** Connected, completed, written, saved.
- **Warning (#ffaa33):** Approaching limits, degraded state.
- **Error (#ff4a12):** Failed, unreachable, invalid. Deliberately the same hue as the brand — context (border placement, toast, icon) makes meaning clear, and the system avoids introducing a second red.
- **Info (#7aaaff):** Updates, links, references.

The Ember and Iron scales below are the source steps; semantic tokens above resolve to one of these.

## Typography

Three typefaces, each with one job. No fourth family is permitted.

- **Display — Barlow Condensed:** Set in 700–900 weights, **always uppercase**, never sentence case. Used for headlines, panel titles, button labels, and the wordmark. The condensed proportions read as institutional and engineered; the uppercase rule keeps display type tight and unambiguous.
- **Body — Barlow:** Set at 14px / 400 by default. Used for prose, descriptions, menu items, and documentation. Sentence case for prose; italic only for inline emphasis, used sparingly.
- **Mono — Fira Code:** Used for everything technical — code, file paths, keyboard shortcuts, error codes, identifiers, and section labels. Programming ligatures (`!=`, `==`, `=>`, `->`, `>=`, `<=`, `::`, `...`) are enabled via `font-feature-settings: "liga" 1, "calt" 1`. Section labels at 9px use `letter-spacing: 0.3em` and uppercase to read as engraved metadata, not running text.

Minimum sizes are locked: Barlow Condensed at 14px, Barlow at 12px, Fira Code at 9px. Buttons and tabs use the `button-label` token (Barlow Condensed 700, 11px, uppercase, `letter-spacing: 0.1em`). Input field labels use the `input-label` token (Fira Code 9px uppercase, `letter-spacing: 0.2em`).

## Layout

The shell is a fixed grid of horizontal bands stacked top-to-bottom: a 32px title bar, the body, and a 22px status bar. The body splits horizontally into a 44px activity bar, a resizable 190px sidebar, and the main canvas. The main canvas owns a 33px tab bar and a flexible pane region that subdivides as a 2×2 quad canvas (`grid: 1fr 1fr / 1fr 1fr`) when more than two panes are open.

Spacing follows a **base-4 scale** — 4, 8, 12, 16, 20, 24, 32, 40, 48 px — with tokens `xs` through `5xl`. The 4px step is the atomic unit; 16px (`lg`) is the standard panel padding; 24px (`2xl`) is the section gap inside panels. There is no fluid grid: panes are flex-resizable but spacing within them is strictly token-driven.

Pane headers are exactly 28px tall, with `0 16px` horizontal padding and a 12px gap between elements. The sidebar defaults to 190px and resizes within a `min-width: 320px` floor for the overall canvas — below that, headers collapse to icons. Layouts persist per-workspace in `.forge/layouts.json`.

## Elevation & Depth

Forge does not use drop shadows to convey hierarchy. **Surface elevation** does that work, in strict order from deepest to most-elevated:

```
iron-900  (#07080a)  app background
iron-850  (#0d0f13)  panels, sidebars
iron-800  (#13161d)  tab bar, cards, dropdowns, pane headers
iron-750  (#181c26)  hover states, selected rows, active tabs
iron-700  (#1c2230)  default borders and dividers
iron-600  (#252f3e)  focused borders, selected borders
```

A surface may never be lighter than a surface stacked above it. Active and selected states sit on iron-750, with the Ember accent appearing only on a 1px left border or underline — never as a fill on top of iron-750.

Shadows are reserved for **floating, transient surfaces only**, where a shadow is needed to detach the surface from whatever it occludes:

- Command palette backdrop: `0 24px 48px rgba(0, 0, 0, 0.5)`
- Status bar popover: `0 8px 20px rgba(0, 0, 0, 0.35)`
- Context chip preview: `0 6px 20px rgba(0, 0, 0, 0.35)`

The system has exactly one **glow**: connected MCP servers show `box-shadow: 0 0 6px rgba(61, 220, 132, 0.5)` to communicate live network connectivity. No other element glows.

Code blocks use `#050709` — slightly darker than `iron-900` — to push them visually behind the page surface. This is the only legal surface darker than the app background, and it exists exclusively for this purpose.

## Shapes

Three corner radii, no others. Forge uses **small radii deliberately** — large radii read as soft, consumer, and approachable, which is the wrong register for a developer workshop.

- **`sm` (3px):** The default. Buttons, inputs, badges, chips, code blocks, tabs.
- **`md` (5px):** Floating chrome. Toasts, dropdowns, status bar popovers.
- **`lg` (8px):** Containers. Cards, modals, the command palette, the shell window.
- **`pill` (10px):** Status bar agent badges only — pill shape signals "running count," distinct from rectangular UI.
- **`full` (9999px):** Reserved for circular indicators (the 6px dirty-dot badge, the connection dot in pane headers).

Do not introduce a `xl` step. Do not soften load-bearing UI.

## Components

All component values resolve to the tokens declared in the YAML frontmatter and `web/packages/design/src/tokens.css`. Component CSS must reference tokens via `var(--token-name)`, never raw hex.

**Buttons.** Four archetypes: `primary`, `ghost`, `danger`, `icon`. All four use the `button-label` typography token (Barlow Condensed 700, 11px, uppercase, `letter-spacing: 0.1em`) and the `sm` radius. Active state always includes `transform: translateY(1px)` to reinforce the press. Disabled state uses **iron-700 background with text-secondary foreground** — readable but clearly muted, communicating "off" without faded opacity. The primary button **darkens on interact** rather than lightening: default Ember 400, hover Ember 500, active Ember 600. Text is locked to `#ffffff` across all three states; the default state lands at 3.37:1 (the locked brand-exception ratio shared with the status bar), hover clears WCAG AA at ~5.05:1, and active clears AA comfortably at ~7.12:1. Focus ring is `outline: 2px solid var(--color-ember-400); outline-offset: 2px` everywhere. Tabs share the button-label typography and adopt iron-750 background plus an Ember 400 border when selected.

**Inputs.** Default border is iron-600. Hover border lifts to iron-300. Focus border is **always Ember 400, without exception**. Error border is also Ember 400 — the placement of the message disambiguates state. Success border is `#3ddc84`. Labels sit above the field in the `input-label` token (Fira Code 9px uppercase).

**Chips.** Two flavors: a basic chip (Fira Code 10px uppercase on iron-850 with a 1px iron-700 border) used for inline metadata, and a context chip (Fira Code 11px on iron-750 with a 1px iron-600 border) used in the chat composer for `@`-references. Provider chips inherit their accent color and border from the provider's identity color.

**Toasts.** A 3px left accent bar in the semantic color, a dark tinted background, and a semantic border. Success and info auto-dismiss at 5s; warning at 8s; **errors persist until the user actions them**. Maximum four visible at once, stacked from bottom-right above the status bar.

**Status bar.** The status bar is the one surface that intentionally violates WCAG AA contrast (~3.35:1) for brand reasons. Its background is locked to Ember 400 across every theme; its text is `#ffffff`; its height is 22px. Agent count badges inside the status bar are pill-shaped (10px radius), 18px tall, with a **solid iron-800 background** — never alpha-on-ember, which produces a muddy non-token color.

**Pane header.** A 28px row with a 10px Fira Code uppercase type label (CHAT, TERMINAL, EDITOR), a Barlow 13px subject (agent name or filename), a provider pill on the left side, and a Fira Code 10px cost meter (`in 1.2k · out 34k · $0.04`) margin-left auto'd to the right. Close text is "CLOSE SESSION" / "CLOSE TAB" / "CLOSE PANE" depending on context.

**Streaming cursor.** A 5px × 12px Ember 400 rectangle with a 1px radius and a 1s blink at 50% duty cycle. Always present during streaming, removed when streaming ends. Forge does not use spinners for streaming — the cursor itself is the indicator.

**Tool-call cards.** Inline in the chat, on a 4%-opacity Ember 100 background with a 15%-opacity Ember 100 border. Always show tool name, truncated arguments, result, and duration. Most recent call is expanded; older calls collapse. Sub-agent banners prefix with `↳ spawned sub-agent:` and indent the sub-thread under a 2px iron-600 left border.

**Loading states.** Skeleton placeholders use a shimmer (`linear-gradient(90deg, iron-800, iron-750, iron-800)`, `background-size: 200% 100%`, `animation: 1.4s linear infinite`). Reduced-motion users see a static iron-750 fill. Forge never uses spinners for content loading.

## Do's and Don'ts

**Visual:**

- **Do** use Ember 400 for the single most important action per view.
- **Don't** use Ember as a decorative accent or sprinkle it across multiple actions.
- **Do** keep border radii at 3px for buttons, inputs, chips, and tabs.
- **Don't** increase radii to "soften" the feel — softness is off-brand.
- **Do** stack surfaces strictly from iron-900 (deepest) to iron-750 (most elevated).
- **Don't** place a lighter surface below a darker one.
- **Do** use the warm off-white `#eae6de` for primary text.
- **Don't** use pure `#ffffff` for body text — it causes eye strain on the dark background.
- **Do** use Barlow Condensed in uppercase for every heading, panel title, and button.
- **Don't** mix casing styles within the same component or context.
- **Do** show streaming with the blinking 5px Ember cursor.
- **Don't** use a spinner to indicate streaming or content loading.

**Tokens:**

- **Do** reference values via `var(--color-…)`, `var(--sp-…)`, `var(--r-…)` in component CSS.
- **Don't** write raw hex values or px values for tokenized properties.
- **Do** keep design tokens in this DESIGN.md and `tokens.css` in sync — `pnpm check-tokens` is the CI gate.
- **Don't** introduce a fourth typeface or a fifth Ember step without an ADR.

**Behavior:**

- **Do** surface every AI action inline as a tool-call card.
- **Don't** silently execute tool calls or hide them in a separate log.
- **Do** label every pane with its active provider via the provider pill.
- **Don't** leave a pane without provider context.
- **Do** persist error toasts until the user actions them.
- **Don't** auto-dismiss errors — the user needs the chance to read and act.
- **Do** keep the status bar background Ember 400 in every theme.
- **Don't** re-color the status bar — the brand exception is locked.
- **Do** use iron-700 background with text-secondary foreground for disabled buttons — muted but readable.
- **Don't** reduce opacity to indicate disabled state on interactive elements, and don't use the same color for text and background.
- **Do** maintain WCAG AA contrast (4.5:1) for body text.
- **Don't** drop below AA for any surface other than the status bar (the documented exception).

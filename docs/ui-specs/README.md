# UI specs

> Behavioral specs for every webview surface — one file per pane / section / dialog. Reviewers diff the rendered component against its spec; missing specs make drift undetectable.

---

## Coverage policy

Every webview component that renders user-visible chrome ships with a paired spec under `docs/ui-specs/`. A component without a spec is treated as a process gap, not a stylistic preference — see [F-686](https://github.com/forge-ide/forge/issues/722) for the precedent.

**Required for:**
- Top-level routes (`Dashboard`, `AgentMonitor`, `UsagePane`, `CatalogPane`, …).
- Dashboard sections (`*Section.tsx` files) and stand-alone panes (`*Pane.tsx`).
- Stand-alone dialogs / flyouts that own their own modal lifecycle (`MemoryEditor`, `RotationConfirm`, `LogsFlyout` …) — but a dialog that lives inside a host section may be specced inline as a sub-section of the host (e.g. `containers-section.md §F.7` covers the logs flyout).
- New cross-cutting interaction patterns surfaced in more than one place (`streaming-states.md`, `pane-header.md`, `provider-selector.md`).

**Not required for:**
- Pure layout primitives in `@forge/design` (e.g. `Button`, `Tabs`, `Skeleton`). Their visual contract lives in `docs/design/` and the package README.
- Internal helper components used by exactly one spec'd parent — those are covered by the parent's structure section.

## Spec shape

Every spec follows the same skeleton so reviewers can diff structurally:

1. **Title + one-line purpose** (block-quoted under the H1).
2. **`Purpose.`** — what user goal the surface serves.
3. **`Where.`** — route / mount point.
4. **`Size.`** — fill / fixed / breakpoint behavior.
5. **`Structure`** — ASCII sketch of the layout, top-to-bottom.
6. **`States`** — loading / empty / error / ready, per `docs/design/component-principles.md` four-state rule.
7. **`Copy`** — verbatim user-facing strings so future drift is flagged in review.
8. **`Color & typography`** — token references, never raw hex.
9. **`Keyboard`** — shortcut and focus contract.
10. **`Cross-spec references`** — links to neighbouring specs.
11. **`Doesn't do.`** — explicit non-goals so future PRs don't re-litigate scope.

`docs/ui-specs/dashboard.md` is the canonical model — mirror its tone (terse, intent-focused, customer-centric per `docs/design/voice-terminology.md`).

## PR review checklist

When reviewing a PR that introduces or substantially changes a webview component:

- [ ] **Spec exists.** A paired `docs/ui-specs/<name>.md` is updated or created in the same PR. If the change is genuinely cosmetic (token swap only, no behavior change) the spec touch may be skipped — call it out in the PR body.
- [ ] **Four states covered.** Loading / empty / error / ready are each documented and the rendered component renders each distinctly (`docs/design/component-principles.md`).
- [ ] **Copy frozen.** Every user-visible string in the spec matches the rendered string verbatim. Drift here is the most common spec rot — flag any mismatch before approval.
- [ ] **No dashboard-section exemption.** Dashboard sections (`web/packages/app/src/components/dashboard/*Section.tsx`) get their own spec file — `dashboard.md` covers the *root* layout only and is not a catch-all section spec.
- [ ] **Cross-spec links resolve.** Markdown links in the new spec point at files that exist; loading-state references defer to `dashboard.md §D.5.1` rather than redefining the skeleton contract.

## Cross-references

- `docs/design/voice-terminology.md` — copy tone and the noun + state placeholder convention.
- `docs/design/component-principles.md` — the four-state rule.
- `docs/design/ai-patterns.md` — interaction-state primitives (skeletons, streaming cursor, provider accent).
- `docs/frontend/architecture.md` — frontend stack and store boundaries.

<!--
Forge PR template. Keep the summary concise; the description's value is
in `why`, not `what` (the diff already covers what changed).
-->

## Summary

<!-- 1-3 bullets describing the intent and the surfaces touched. -->

## Definition of Done

<!-- Mirror the issue's DoD checklist; tick each item as it lands. -->

- [ ] …

## Test plan

<!-- Bulleted list of the commands / manual checks that prove the change
     works. CI mirrors `just check-rust` / `just check-web` / `just
     test-rust` / `just test-web`; spell out anything beyond that. -->

- [ ] …

## Frontend checklist

<!-- Skip if this PR is backend-only. Otherwise tick each that applies. -->

- [ ] If this PR adds or changes a frontend component, it has a paired
      entry under `docs/ui-specs/` (or extends an existing spec).
- [ ] Design tokens: no raw hex / px in component CSS or inline styles —
      `pnpm check-tokens` is clean.
- [ ] No raw `<button>` outside `web/packages/design/` — `pnpm
      check-raw-buttons` is clean.
- [ ] Voice & terminology: copy follows `docs/design/voice-terminology.md`.

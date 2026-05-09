# @forge/design

The Forge design-token package. Ships a single static CSS file (`src/tokens.css`) that defines every CSS custom property the UI consumes — the Ember brand scale, Iron surface scale, text colours, semantic states and backgrounds, plus the rest of the typography and spacing scale referenced from `docs/design/token-reference.md`. The package has no runtime: consumers import `@forge/design/tokens.css` once at the app shell and read the variables from their own component CSS.

## Role in the workspace

- Depended on by: `app` (imports `@forge/design/tokens.css`).
- Depends on: nothing.

## Key types / entry points

- `src/tokens.css` — the single source of truth; mirrored from `docs/design/token-reference.md` and enforced by `scripts/check-tokens.mjs`. Edits to either side without the other will fail the token check.
- `package.json` `exports` — `./tokens.css` is the only public entry; `files` ships exactly that file.
- Scripts: `build`, `test`, and `typecheck` are intentional no-ops (static CSS, no compile step).

## Further reading

- [Token pipeline](../../../docs/frontend/architecture.md#94-design-token-pipeline)
- [Frontend architecture](../../../docs/frontend/architecture.md)
- [Crate architecture overview](../../../docs/architecture/crate-architecture.md)

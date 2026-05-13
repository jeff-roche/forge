#!/usr/bin/env node
// Raw-`<button>` drift check — ACTIVE (F-699 follow-up #820, item 4).
//
// Forbids raw `<button>` JSX inside `web/packages/app/src/` outside the
// allowlisted row/card/popover sites where the content shape isn't cleanly
// expressible through a `@forge/design` primitive (Button / IconButton /
// Tab / MenuItem).
//
// History: drafted disabled in F-398 alongside the migration plan; flipped
// active in #820 once the Phase 3 primitives shipped. The allowlist tracks
// the residual sites where a primitive doesn't fit yet — those are tracked
// for migration in a sibling cleanup issue, not blocked by this rule.
//
// Skips:
//   - `.test.tsx` and `*.test.ts` files (tests legitimately reference
//     raw `<button>` strings, e.g. asserting "this used to be a raw
//     button and now isn't").
//   - Files whose path matches an entry in `ALLOWLIST` (relative to
//     `web/packages/app/src/`).
//
// Migration plan: `docs/frontend/button-primitives-migration.md`.
// Tests:         `scripts/check-raw-buttons.test.mjs` (unit-tested on fixtures).

import { readdirSync, readFileSync } from 'node:fs';
import { relative, resolve } from 'node:path';

// Allowlist of files that legitimately render a raw `<button>` because the
// content shape (row, card, tree item, popover-internal action) isn't
// cleanly expressible through a primitive yet. Migration to design
// primitives is tracked in a sibling cleanup issue.
//
// Paths are relative to the repo root, using forward slashes.
export const ALLOWLIST = [
  'web/packages/app/src/shell/StatusBar.tsx',                  // bg-agent badge row
  'web/packages/app/src/components/BranchMetadataPopover.tsx', // variant rows
  'web/packages/app/src/routes/AgentMonitor.tsx',              // agent row + trace step
  'web/packages/app/src/routes/Dashboard/SessionsPanel.tsx',   // session card
  // F-699 follow-up #820 baseline — row/card/popover sites still
  // pending design-primitive migration:
  'web/packages/app/src/components/ContextChip.tsx',           // chip-internal retry
  'web/packages/app/src/components/SubAgentBanner.tsx',        // state chip
  'web/packages/app/src/components/SubAgentDetailsPopover.tsx',// popover footer action
  'web/packages/app/src/routes/Session/ChatPane.tsx',          // tool-call card "show more"
  // ARIA combobox trigger — needs custom label/chevron layout and listbox
  // semantics that a Button primitive would override visually.
  'web/packages/app/src/components/Dropdown.tsx',              // combobox trigger
];

/** Recursively yield absolute paths of non-test `.tsx` files under `dir`. */
function* walkTsx(dir) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (entry.name === 'node_modules' || entry.name.startsWith('.')) continue;
    const full = resolve(dir, entry.name);
    if (entry.isDirectory()) {
      yield* walkTsx(full);
    } else if (
      entry.isFile() &&
      entry.name.endsWith('.tsx') &&
      !entry.name.endsWith('.test.tsx')
    ) {
      yield full;
    }
  }
}

/**
 * Match opening JSX `<button` tags. The trailing boundary is a whitespace
 * char or `>` so we don't false-positive on identifiers like `Button`,
 * `ButtonGroup`, etc. (JSX tag names are case-sensitive; capital `B`
 * components are primitives by convention.)
 */
const rawButton = /<button(?=[\s>])/g;

/**
 * Scan every `.tsx` file under `root`, returning `{ file, line }` for each
 * raw `<button` opening tag in a file not covered by `allowlist`.
 *
 * Exported so `check-raw-buttons.test.mjs` can drive it with tmpdir
 * fixtures without hitting the real repo tree.
 *
 * @param {{ root: string, allowlist?: string[] }} opts
 * @returns {Array<{ file: string, line: number }>}
 */
export function scanTsxSources({ root, allowlist = [] }) {
  const suppressed = new Set(allowlist);
  const findings = [];
  for (const file of walkTsx(root)) {
    const rel = relative(root, file).split('\\').join('/');
    if (suppressed.has(rel)) continue;
    const source = readFileSync(file, 'utf-8');
    // Strip `//` line comments before scanning so prose mentions of
    // `<button>` in comments aren't flagged. Block comments and string
    // literals are out of scope for now — JSX `<button` inside a string
    // literal is implausible and the rule has explicit unit-test
    // coverage for the cases we care about.
    const scanSource = source.replace(/^\s*\/\/.*$/gm, '');
    rawButton.lastIndex = 0;
    let m;
    while ((m = rawButton.exec(scanSource)) !== null) {
      const upto = scanSource.slice(0, m.index);
      const line = upto.split('\n').length;
      findings.push({ file: rel, line });
    }
  }
  return findings;
}

/** CLI entry point — invoked only when this file is run directly. */
function main() {
  const repoRoot = resolve(new URL('..', import.meta.url).pathname);
  const scanRoot = resolve(repoRoot, 'web/packages/app/src');
  const findings = scanTsxSources({ root: scanRoot, allowlist: ALLOWLIST.map((p) => relative('web/packages/app/src', p).split('\\').join('/')) });
  if (findings.length > 0) {
    console.error(`Raw <button> detected at ${findings.length} site(s):`);
    for (const f of findings) console.error(`  - ${f.file}:${f.line}`);
    console.error('\nUse a @forge/design primitive (Button / IconButton / Tab / MenuItem)');
    console.error('or add the file to ALLOWLIST in scripts/check-raw-buttons.mjs if it');
    console.error('is a row/card-as-button site.');
    process.exit(1);
  }
  console.log('ok: no raw <button> outside allowlisted sites');
}

// Only run the CLI when this file is invoked directly (not when imported by tests).
if (import.meta.url === `file://${process.argv[1]}`) {
  main();
}

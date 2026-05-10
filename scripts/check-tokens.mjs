#!/usr/bin/env node
// Design token drift check.
// Enforces that web/packages/design/src/tokens.css matches the CSS custom
// properties declared inside the ```css fenced block in
// docs/design/token-reference.md (the authoritative source).
//
// Documented in docs/frontend/generation-pipelines.md (§1, "Design tokens").
// Invoked via `pnpm --filter forge-web run check-tokens` or directly:
// `node scripts/check-tokens.mjs`.

import { readdirSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, relative, resolve } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..');
const refPath = resolve(repoRoot, 'docs/design/token-reference.md');
const cssPath = resolve(repoRoot, 'web/packages/design/src/tokens.css');
// Scope for the inline-style scan (F-389): the webview's own TSX sources.
// Any raw px/hex value inside a JSX `style={...}` block here should live in
// tokens.css or a `.css` class instead.
const tsxScanRoots = [
  resolve(repoRoot, 'web/packages/app/src'),
  resolve(repoRoot, 'web/packages/design/src'),
];

// Scope for the CSS-side typography scan (F-699 follow-up #820, item 1).
// We flag bare `font-size: <N>px` and `line-height: <N>px` declarations —
// the two CSS properties where pixel literals most often drift from the
// typography scale. Static contexts where px is intentional (border-width,
// margin, padding, transform) are *not* covered: those have their own
// design-time conventions and the false-positive rate would dominate.
const cssScanRoots = [
  resolve(repoRoot, 'web/packages/app/src'),
  resolve(repoRoot, 'web/packages/design/src'),
];

// Pre-existing violations baseline (F-699 follow-up #820). 29 files at the
// time of landing the rule; cleanup is tracked separately so this PR can
// ship the gate without an unrelated style migration. Do NOT add new
// entries — fix the violation instead, or use a typography token.
const cssTypographyAllowlist = new Set([
  'web/packages/app/src/components/ApprovalPrompt/ApprovalPrompt.css',
  'web/packages/app/src/components/ApprovalPrompt/WhitelistedPill.css',
  'web/packages/app/src/components/BranchMetadataPopover.css',
  'web/packages/app/src/components/BranchSelectorStrip.css',
  'web/packages/app/src/components/catalog/CatalogPane.css',
  'web/packages/app/src/components/ContextChip.css',
  'web/packages/app/src/components/ContextPicker.css',
  'web/packages/app/src/components/dashboard/ContainersSection.css',
  'web/packages/app/src/components/dashboard/CredentialsSection.css',
  'web/packages/app/src/components/dashboard/MemorySection.css',
  'web/packages/app/src/components/dashboard/ProvidersSection.css',
  'web/packages/app/src/components/RerunPopover.css',
  'web/packages/app/src/components/SubAgentBanner.css',
  'web/packages/app/src/components/SubAgentDetailsPopover.css',
  'web/packages/app/src/components/usage/UsagePane.css',
  'web/packages/app/src/panes/EditorPane.css',
  'web/packages/app/src/panes/TerminalPane.css',
  'web/packages/app/src/routes/AgentMonitor.css',
  'web/packages/app/src/routes/Catalog.css',
  'web/packages/app/src/routes/Dashboard/ProviderPanel.css',
  'web/packages/app/src/routes/Dashboard/SessionsPanel.css',
  'web/packages/app/src/routes/Session/ChatPane.css',
  'web/packages/app/src/routes/Session/CompactButton.css',
  'web/packages/app/src/routes/Session/PaneHeader.css',
  'web/packages/app/src/routes/Session/SessionWindow.css',
]);

/**
 * Extract `--name: value;` declarations from a CSS string, preserving
 * original order and normalising whitespace inside values.
 * @param {string} source
 * @returns {Map<string, string>}
 */
function parseTokens(source) {
  const tokens = new Map();
  const re = /(--[a-z0-9-]+)\s*:\s*([^;]+);/gi;
  let m;
  while ((m = re.exec(source)) !== null) {
    const name = m[1];
    const value = m[2].trim().replace(/\s+/g, ' ');
    tokens.set(name, value);
  }
  return tokens;
}

/** Extract the first ```css fenced block from a markdown document. */
function extractCssBlock(markdown) {
  const match = markdown.match(/```css\n([\s\S]*?)```/);
  if (!match) {
    throw new Error(`No \`\`\`css block found in ${refPath}`);
  }
  return match[1];
}

const refMarkdown = readFileSync(refPath, 'utf-8');
const cssSource = readFileSync(cssPath, 'utf-8');

const referenceTokens = parseTokens(extractCssBlock(refMarkdown));
const cssTokens = parseTokens(cssSource);

const errors = [];

for (const [name, value] of referenceTokens) {
  if (!cssTokens.has(name)) {
    errors.push(`missing in tokens.css: ${name}`);
  } else if (cssTokens.get(name) !== value) {
    errors.push(`value drift: ${name} — reference="${value}" css="${cssTokens.get(name)}"`);
  }
}

for (const name of cssTokens.keys()) {
  if (!referenceTokens.has(name)) {
    errors.push(`extra in tokens.css (not in reference): ${name}`);
  }
}

// ---------------------------------------------------------------------------
// F-389: scan .tsx files for raw px/hex literals inside JSX `style={...}`
// blocks. Inline styling is the escape hatch that lets raw values bypass
// tokens.css, so the gate must cover it.
//
// Heuristic: `\d+px` / `\d*\.\d+px` matches *adjacent* digit+unit pairs only,
// so template-literal interpolations like `${expr}px` (runtime-computed
// positions) don't trip it. Hex: `#[0-9a-fA-F]{3,8}` catches static colors.
// ---------------------------------------------------------------------------

/** Recursively yield absolute paths of `.tsx` files under `dir`. */
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
    } else if (entry.isFile() && entry.name.endsWith('.tsx')) {
      yield full;
    }
  }
}

/** Recursively yield absolute paths of `.css` files under `dir`. */
function* walkCss(dir) {
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
      yield* walkCss(full);
    } else if (entry.isFile() && entry.name.endsWith('.css')) {
      yield full;
    }
  }
}

/**
 * Yield every JSX `style={...}` block body (the text between the outer
 * braces) as `{ body, offset }` where `offset` is the start index in the
 * source. Handles nested braces inside object literals / template strings.
 */
function* extractStyleBlocks(source) {
  const re = /style\s*=\s*\{/g;
  let m;
  while ((m = re.exec(source)) !== null) {
    const start = m.index + m[0].length; // position just after the opening `{`
    let depth = 1;
    let i = start;
    while (i < source.length && depth > 0) {
      const ch = source[i];
      if (ch === '{') depth += 1;
      else if (ch === '}') depth -= 1;
      i += 1;
    }
    if (depth === 0) yield { body: source.slice(start, i - 1), offset: start };
  }
}

/** 1-based (line, column) for a character offset in `source`. */
function locate(source, offset) {
  let line = 1;
  let col = 1;
  for (let i = 0; i < offset; i += 1) {
    if (source[i] === '\n') {
      line += 1;
      col = 1;
    } else {
      col += 1;
    }
  }
  return { line, col };
}

const rawPx = /\d+(?:\.\d+)?px/;
const rawHex = /#[0-9a-fA-F]{3,8}\b/;

for (const root of tsxScanRoots) {
  for (const file of walkTsx(root)) {
    const source = readFileSync(file, 'utf-8');
    for (const { body, offset } of extractStyleBlocks(source)) {
      const pxMatch = body.match(rawPx);
      const hexMatch = body.match(rawHex);
      const rel = relative(repoRoot, file);
      if (pxMatch) {
        const { line } = locate(source, offset + pxMatch.index);
        errors.push(
          `raw px in inline style (use tokens.css or a CSS class): ${rel}:${line} — ${pxMatch[0]}`,
        );
      }
      if (hexMatch) {
        const { line } = locate(source, offset + hexMatch.index);
        errors.push(
          `raw hex in inline style (use tokens.css or a CSS class): ${rel}:${line} — ${hexMatch[0]}`,
        );
      }
    }
  }
}

// ---------------------------------------------------------------------------
// F-699 follow-up #820 (item 1): scan component CSS for raw `font-size: <N>px`
// and `line-height: <N>px`. These are the two declarations where pixel
// drift hurts the typography scale; other px declarations (border-width,
// margin, transform) are intentional and out of scope.
//
// Skip token files (the design package owns the source of truth for sizing
// custom properties) and skip declarations sourced from a CSS variable —
// `font-size: var(--type-body)` resolves to a px value at runtime, but the
// literal text is not a raw px. We match the literal `<N>px` *only* on the
// right-hand side of `font-size:` / `line-height:`.
// ---------------------------------------------------------------------------

// Boundary `(?<![-a-zA-Z])` keeps us from false-positiving on hyphenated
// custom properties like `--my-font-size: 13px;` (those are sizing tokens
// in their own right, not bare declarations).
const cssTypographyPxRe = /(?<![-a-zA-Z])(font-size|line-height)\s*:\s*\d+(?:\.\d+)?px\b/g;

for (const root of cssScanRoots) {
  for (const file of walkCss(root)) {
    const rel = relative(repoRoot, file).split('\\').join('/');
    // Tokens.css is the source of truth for sizing custom properties — px
    // literals here define the scale that other files consume.
    if (rel.endsWith('/tokens.css')) continue;
    if (cssTypographyAllowlist.has(rel)) continue;
    const source = readFileSync(file, 'utf-8');
    cssTypographyPxRe.lastIndex = 0;
    let m;
    while ((m = cssTypographyPxRe.exec(source)) !== null) {
      const upto = source.slice(0, m.index);
      const line = upto.split('\n').length;
      errors.push(
        `raw ${m[1]} px in CSS (use a typography token): ${rel}:${line} — ${m[0]}`,
      );
    }
  }
}

if (errors.length > 0) {
  console.error('Token drift detected:');
  for (const e of errors) console.error(`  - ${e}`);
  console.error(`\nReference: ${refPath}`);
  console.error(`CSS:       ${cssPath}`);
  process.exit(1);
}

console.log(`ok: ${cssTokens.size} tokens match ${refPath}`);

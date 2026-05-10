// F-427 drift guard + F-392 ember-400 spec-lock for StatusBar.css.
//
// The file previously referenced a `--fg-*` token namespace that was never
// declared in `web/packages/design/src/tokens.css` or
// `docs/design/token-reference.md`, with raw-hex fallbacks bypassing the
// token enforcement invariant documented in `docs/design/color-system.md`.
// These tests fail fast if either regression returns — any custom property
// used in the file must belong to the canonical `--color-*` / `--sp-*` /
// `--r-*` / `--font-*` / `--ease` namespace, and no raw color fallbacks
// may be supplied to `var(...)`.
//
// F-392 additionally pins the ember-400 spec-lock from
// `docs/design/component-principles.md §Status bar` and `shell.md §2`:
// "The status bar is always ember-400 background with white text … Do not
// change the status bar color under any circumstances." The `.status-bar`
// root rule must set `background: var(--color-ember-400)` and must NOT
// reintroduce any `--color-surface-*` or `--fg-surface-*` fallback on the
// background. Mirrors the `ActivityBar.css.test.ts` targeted-rule-match
// pattern (PR #469).
//
// F-392-followup adds the WCAG AA contrast audit. The spec-lock and WCAG AA
// conflict at the per-component level; resolution (per `color-system.md
// §Brand exception — status bar`) is:
//   - Bar body: ember-400 + white is a documented brand exception. The
//     computed contrast (~3.35:1) is pinned as a regression floor — if a
//     future palette change drops it further, this test fails.
//   - Interactive controls on the bar (the `__bg-badge`) are NOT covered by
//     the brand exception — they are essential interactive text, so WCAG
//     AA normal-text 4.5:1 applies. The badge uses a solid iron chip
//     (surface-2) so its text/background contrast is unambiguously AA.
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const cssPath = resolve(__dirname, 'StatusBar.css');
const source = readFileSync(cssPath, 'utf-8');

const tokensPath = resolve(
  __dirname,
  '../../../../../web/packages/design/src/tokens.css',
);
const tokensSource = readFileSync(tokensPath, 'utf-8');

const CANONICAL_PREFIXES = ['--color-', '--sp-', '--r-', '--font-', '--type-', '--ease'];

interface VarCall {
  name: string;
  fallback: string | null;
  raw: string;
}

/**
 * Extract every `var(--name[, fallback])` call from the CSS source.
 * Handles nested `var(..., var(...))` by walking parentheses.
 */
function collectVarCalls(src: string): VarCall[] {
  const calls: VarCall[] = [];
  const re = /var\(\s*(--[a-zA-Z0-9_-]+)\s*(,([^)]|\([^)]*\))*)?\)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(src)) !== null) {
    const name = m[1] ?? '';
    const fallback = m[2] ? m[2].replace(/^,\s*/, '').trim() : null;
    calls.push({ name, fallback, raw: m[0] });
  }
  return calls;
}

describe('StatusBar.css token hygiene (F-427)', () => {
  const calls = collectVarCalls(source);

  it('references at least one custom property (sanity check)', () => {
    expect(calls.length).toBeGreaterThan(0);
  });

  it('uses only canonical design tokens — no --fg-* or other ad-hoc namespaces', () => {
    const offenders = calls
      .filter(({ name }) => !CANONICAL_PREFIXES.some((p) => name.startsWith(p)))
      .map(({ raw }) => raw);
    expect(offenders).toEqual([]);
  });

  it('does not supply raw-hex or rgb/rgba fallbacks to var()', () => {
    const rawValue = /^(#[0-9a-fA-F]{3,8}|rgba?\()/;
    const offenders = calls
      .filter(({ fallback }) => fallback !== null && rawValue.test(fallback!))
      .map(({ raw }) => raw);
    expect(offenders).toEqual([]);
  });
});

describe('StatusBar.css ember-400 spec-lock (F-392)', () => {
  // Targeted-rule-match pattern mirroring ActivityBar.css.test.ts (PR #469):
  // parse the `.status-bar { ... }` root rule body directly and assert against
  // it. jsdom does not resolve external stylesheets at render time, so the
  // CSS source file is the contract.
  const rootMatch = source.match(/\.status-bar\s*\{([^}]*)\}/);

  it('declares a root `.status-bar` rule', () => {
    expect(rootMatch).not.toBeNull();
  });

  it('sets background to --color-ember-400 (spec-lock)', () => {
    const body = rootMatch?.[1] ?? '';
    expect(body).toMatch(/background\s*:\s*var\(--color-ember-400\)/);
  });

  it('does not fall back to a neutral surface token on background', () => {
    const body = rootMatch?.[1] ?? '';
    // No --color-surface-*, --fg-surface-*, or --color-bg anywhere in the
    // root rule — the ember background must be unconditional.
    expect(body).not.toMatch(/--color-surface-/);
    expect(body).not.toMatch(/--fg-surface-/);
    expect(body).not.toMatch(/--color-bg\b/);
  });

  it('uses --color-text-inverted (white) for the bar text color', () => {
    const body = rootMatch?.[1] ?? '';
    expect(body).toMatch(/color\s*:\s*var\(--color-text-inverted\)/);
  });

  it('does not reintroduce a neutral top border (redundant on ember)', () => {
    const body = rootMatch?.[1] ?? '';
    expect(body).not.toMatch(/border-top\s*:/);
  });
});

// ---------------------------------------------------------------------------
// F-392-followup: WCAG AA contrast audit.
// ---------------------------------------------------------------------------
//
// These helpers implement the WCAG 2.1 contrast algorithm from scratch against
// the canonical token palette. Pinning the numbers in a unit test means the
// brand-exception rationale in `color-system.md` is grounded in a machine-
// checked floor: if the palette shifts, the test surfaces exactly which
// surface/text pair regressed.

interface Rgb {
  r: number;
  g: number;
  b: number;
}

function parseHex(hex: string): Rgb {
  const h = hex.replace(/^#/, '');
  const expand =
    h.length === 3
      ? h.split('').map((c) => c + c).join('')
      : h.length === 8
        ? h.slice(0, 6)
        : h;
  const n = parseInt(expand, 16);
  return { r: (n >> 16) & 0xff, g: (n >> 8) & 0xff, b: n & 0xff };
}

function readTokenHex(name: string): string {
  const re = new RegExp(`${name}\\s*:\\s*(#[0-9a-fA-F]{3,8})`);
  const m = tokensSource.match(re);
  const hex = m?.[1];
  if (!hex) throw new Error(`token ${name} not found`);
  return hex;
}

function srgbToLinear(channel: number): number {
  const c = channel / 255;
  return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

function relativeLuminance({ r, g, b }: Rgb): number {
  return (
    0.2126 * srgbToLinear(r) +
    0.7152 * srgbToLinear(g) +
    0.0722 * srgbToLinear(b)
  );
}

function contrast(a: Rgb, b: Rgb): number {
  const la = relativeLuminance(a);
  const lb = relativeLuminance(b);
  const [hi, lo] = la > lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

describe('StatusBar.css WCAG AA contrast audit (F-392-followup)', () => {
  it('bar body: ember-400 × text-inverted meets the documented brand-exception floor', () => {
    // Spec-lock wins over AA for the bar body — the status bar is a brand
    // element, sized at 22px height, and cannot satisfy AA 4.5:1 without
    // abandoning ember-400. `color-system.md` formally accepts this as a
    // brand exception; this test pins the computed contrast so any palette
    // drift that WORSENS the ratio trips immediately.
    const ember = parseHex(readTokenHex('--color-ember-400'));
    const white = parseHex(readTokenHex('--color-text-inverted'));
    const ratio = contrast(ember, white);
    expect(ratio).toBeCloseTo(3.35, 1);
    expect(ratio).toBeGreaterThanOrEqual(3.0); // WCAG AA Non-text / Large floor
  });

  it('badge: surface-2 × text-primary clears WCAG AA 4.5:1 normal-text', () => {
    // The bg-agents badge is NOT covered by the brand exception — it is
    // interactive, essential, and reports live state. Per F-392-followup it
    // renders on a solid iron chip so its contrast can be audited against
    // a fixed pair (no alpha composite over ember).
    const surface = parseHex(readTokenHex('--color-surface-2'));
    const text = parseHex(readTokenHex('--color-text-primary'));
    const ratio = contrast(surface, text);
    expect(ratio).toBeGreaterThanOrEqual(4.5);
  });

  it('badge rule declares a solid iron background — no translucent-white overlay on ember', () => {
    // Anchors the CSS: the badge MUST NOT regress to `rgba(255,255,255,...)`
    // composited over ember-400, which historically produced ~2.7:1 and was
    // the proximate cause of this follow-up.
    const badgeMatch = source.match(/\.status-bar__bg-badge\s*\{([^}]*)\}/);
    expect(badgeMatch).not.toBeNull();
    const body = badgeMatch?.[1] ?? '';
    expect(body).toMatch(/background\s*:\s*var\(--color-surface-2\)/);
    expect(body).not.toMatch(/rgba\(\s*255\s*,\s*255\s*,\s*255/);
  });
});

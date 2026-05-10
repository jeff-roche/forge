import { describe, expect, it } from 'vitest';
import { execFileSync, spawnSync } from 'node:child_process';
import { readFileSync, unlinkSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

const repoRoot = resolve(__dirname, '../../../..');
const script = resolve(repoRoot, 'scripts/check-tokens.mjs');
const tokensCss = resolve(repoRoot, 'web/packages/design/src/tokens.css');

describe('scripts/check-tokens.mjs', () => {
  it('exits 0 when tokens.css matches docs/design/token-reference.md', () => {
    // Should not throw
    const out = execFileSync('node', [script], { cwd: repoRoot, encoding: 'utf-8' });
    expect(out).toContain('ok');
  });

  it('exits non-zero when tokens.css drifts from the reference doc', () => {
    const original = readFileSync(tokensCss, 'utf-8');
    const mutated = original.replace('--color-ember-400: #ff4a12;', '--color-ember-400: #000000;');
    writeFileSync(tokensCss, mutated);
    try {
      const result = spawnSync('node', [script], { cwd: repoRoot, encoding: 'utf-8' });
      expect(result.status).not.toBe(0);
      expect(result.stderr + result.stdout).toMatch(/drift|mismatch|--color-ember-400/i);
    } finally {
      writeFileSync(tokensCss, original);
    }
  });

  // F-389: the gate must also scan `.tsx` files for raw px/hex literals inside
  // inline `style={...}` blocks. Inline styling is the escape hatch that lets
  // raw values bypass tokens.css; the gate catches that class of drift.
  it('exits non-zero when a .tsx inline style contains a raw px value', () => {
    const fixture = resolve(
      repoRoot,
      'web/packages/app/src/__f389_rawpx_fixture__.tsx',
    );
    writeFileSync(
      fixture,
      [
        "import type { Component } from 'solid-js';",
        'export const Bad: Component = () => (',
        "  <div style={{ 'min-width': '360px' }} />",
        ');',
        '',
      ].join('\n'),
    );
    try {
      const result = spawnSync('node', [script], { cwd: repoRoot, encoding: 'utf-8' });
      expect(result.status).not.toBe(0);
      expect(result.stderr + result.stdout).toMatch(/__f389_rawpx_fixture__\.tsx/);
      expect(result.stderr + result.stdout).toMatch(/360px/);
    } finally {
      unlinkSync(fixture);
    }
  });

  it('exits non-zero when a .tsx inline style contains a raw #hex color', () => {
    const fixture = resolve(
      repoRoot,
      'web/packages/app/src/__f389_rawhex_fixture__.tsx',
    );
    writeFileSync(
      fixture,
      [
        "import type { Component } from 'solid-js';",
        'export const Bad: Component = () => (',
        "  <div style={{ color: '#ff00aa' }} />",
        ');',
        '',
      ].join('\n'),
    );
    try {
      const result = spawnSync('node', [script], { cwd: repoRoot, encoding: 'utf-8' });
      expect(result.status).not.toBe(0);
      expect(result.stderr + result.stdout).toMatch(/__f389_rawhex_fixture__\.tsx/);
      expect(result.stderr + result.stdout).toMatch(/#ff00aa/i);
    } finally {
      unlinkSync(fixture);
    }
  });

  // F-699 followup (#820, item 1): the CSS-side scan must flag bare
  // `font-size: <N>px` and `line-height: <N>px` declarations. Component
  // CSS should reach for the typography tokens (or `rem`) instead of
  // hardcoded pixel sizes — those are the two declarations where px
  // most often drifts, while `border-width: 1px` etc. are intentional.
  it('exits non-zero when a non-allowlisted .css contains a raw font-size px', () => {
    const fixture = resolve(
      repoRoot,
      'web/packages/app/src/__f699_rawfontsize_fixture__.css',
    );
    writeFileSync(fixture, '.x { font-size: 13px; }\n');
    try {
      const result = spawnSync('node', [script], { cwd: repoRoot, encoding: 'utf-8' });
      expect(result.status).not.toBe(0);
      expect(result.stderr + result.stdout).toMatch(/__f699_rawfontsize_fixture__\.css/);
      expect(result.stderr + result.stdout).toMatch(/13px/);
    } finally {
      unlinkSync(fixture);
    }
  });

  it('exits non-zero when a non-allowlisted .css contains a raw line-height px', () => {
    const fixture = resolve(
      repoRoot,
      'web/packages/app/src/__f699_rawlineheight_fixture__.css',
    );
    writeFileSync(fixture, '.y { line-height: 20px; }\n');
    try {
      const result = spawnSync('node', [script], { cwd: repoRoot, encoding: 'utf-8' });
      expect(result.status).not.toBe(0);
      expect(result.stderr + result.stdout).toMatch(/__f699_rawlineheight_fixture__\.css/);
      expect(result.stderr + result.stdout).toMatch(/20px/);
    } finally {
      unlinkSync(fixture);
    }
  });

  it('does not flag border-width or margin px in CSS — only font-size / line-height', () => {
    const fixture = resolve(
      repoRoot,
      'web/packages/app/src/__f699_borderpx_fixture__.css',
    );
    writeFileSync(fixture, '.z { border-width: 1px; margin-bottom: 4px; padding: 8px; }\n');
    try {
      const result = spawnSync('node', [script], { cwd: repoRoot, encoding: 'utf-8' });
      expect(result.status).toBe(0);
    } finally {
      unlinkSync(fixture);
    }
  });
});

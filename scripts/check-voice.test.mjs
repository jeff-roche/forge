#!/usr/bin/env node
// Tests for check-voice.mjs.
//
// Tests scope: drive the script with a fixture file in the repo's scan
// roots, capture stdout, then clean up. Informational mode means the
// script always exits 0; the assertion target is the printed findings.

import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const here = dirname(fileURLToPath(import.meta.url));
const script = resolve(here, 'check-voice.mjs');
const repoRoot = resolve(here, '..');
const scanRoot = resolve(repoRoot, 'web/packages/app/src');

/** Run the script and return { status, stdout, stderr }. */
function run() {
  return spawnSync('node', [script], { cwd: repoRoot, encoding: 'utf-8' });
}

const cases = [];
function test(name, fn) { cases.push({ name, fn }); }

test('exits 0 even when findings present (informational mode)', () => {
  const fixture = resolve(scanRoot, '__voice_test_fixture__.tsx');
  writeFileSync(fixture, 'export const X = () => <p>The model is ready</p>;\n');
  try {
    const result = run();
    if (result.status !== 0) throw new Error(`expected exit 0, got ${result.status}`);
  } finally {
    rmSync(fixture, { force: true });
  }
});

test('flags "AI model" in TSX prose', () => {
  const fixture = resolve(scanRoot, '__voice_aimodel_fixture__.tsx');
  writeFileSync(fixture, 'export const X = () => <p>Choose your AI model</p>;\n');
  try {
    const result = run();
    if (!result.stdout.includes('__voice_aimodel_fixture__.tsx')) {
      throw new Error(`expected fixture in stdout, got:\n${result.stdout}`);
    }
    if (!result.stdout.includes('AI model')) {
      throw new Error(`expected "AI model" match in stdout, got:\n${result.stdout}`);
    }
  } finally {
    rmSync(fixture, { force: true });
  }
});

test('flags "conversation" in TSX prose', () => {
  const fixture = resolve(scanRoot, '__voice_conv_fixture__.tsx');
  writeFileSync(fixture, 'export const X = () => <p>Start a new conversation</p>;\n');
  try {
    const result = run();
    if (!result.stdout.includes('__voice_conv_fixture__.tsx')) {
      throw new Error(`expected fixture in stdout, got:\n${result.stdout}`);
    }
  } finally {
    rmSync(fixture, { force: true });
  }
});

test('does not flag "conversation" in design-doc prose (.md outside TSX)', () => {
  // The conversation rule is TSX-only; long-form design docs may use the
  // word descriptively without violating UI voice.
  const fixture = resolve(repoRoot, 'docs/design/__voice_md_fixture__.md');
  writeFileSync(fixture, '# Note\n\nThe conversation is a UI affordance.\n');
  try {
    const result = run();
    // The fixture path may show up with other rule matches; assert that
    // the conversation rule specifically did NOT fire.
    const lines = result.stdout.split('\n');
    const offending = lines.filter(
      (l) => l.includes('__voice_md_fixture__.md') && l.includes('conversation-as-ui-noun'),
    );
    if (offending.length !== 0) {
      throw new Error(`expected conversation rule to skip .md, got:\n${offending.join('\n')}`);
    }
  } finally {
    rmSync(fixture, { force: true });
  }
});

test('flags "plugin server"', () => {
  const fixture = resolve(scanRoot, '__voice_pluginsrv_fixture__.tsx');
  writeFileSync(fixture, 'export const X = () => <p>Connect a plugin server</p>;\n');
  try {
    const result = run();
    if (!result.stdout.includes('__voice_pluginsrv_fixture__.tsx')) {
      throw new Error(`expected fixture in stdout, got:\n${result.stdout}`);
    }
  } finally {
    rmSync(fixture, { force: true });
  }
});

test('does not flag identifier surfaces like provider.model', () => {
  const fixture = resolve(scanRoot, '__voice_ident_fixture__.tsx');
  writeFileSync(
    fixture,
    'export const X = (props: { provider: { model: string } }) => <p>{props.provider.model}</p>;\n',
  );
  try {
    const result = run();
    const offending = result.stdout
      .split('\n')
      .filter((l) => l.includes('__voice_ident_fixture__.tsx'));
    if (offending.length !== 0) {
      throw new Error(`expected no findings for identifier surface, got:\n${offending.join('\n')}`);
    }
  } finally {
    rmSync(fixture, { force: true });
  }
});

let failed = 0;
for (const { name, fn } of cases) {
  try {
    fn();
    console.log(`ok: ${name}`);
  } catch (err) {
    failed += 1;
    console.error(`FAIL: ${name}`);
    console.error(`  ${err.message}`);
  }
}

if (failed > 0) {
  console.error(`\n${failed} test(s) failed`);
  process.exit(1);
}
console.log(`\n${cases.length} tests passed`);

import { describe, expect, it } from 'vitest';
import {
  adaptContextBlocks,
  providerFlavour,
  toAnthropicXml,
  toOpenAiFunctionContext,
  type ContextBlock,
  type ProviderId,
} from '@forge/ipc';

describe('context-adapter — single block serializers', () => {
  it('toAnthropicXml wraps a file block with type + path attributes', () => {
    const block: ContextBlock = {
      type: 'file',
      path: 'src/app.ts',
      content: "console.log('hi');",
    };
    expect(toAnthropicXml(block)).toBe(
      `<context type="file" path="src/app.ts">\nconsole.log('hi');\n</context>`,
    );
  });

  it('toAnthropicXml omits the path attribute when path is empty', () => {
    const block: ContextBlock = { type: 'selection', content: 'snippet' };
    expect(toAnthropicXml(block)).toBe(
      `<context type="selection">\nsnippet\n</context>`,
    );
  });

  it('toAnthropicXml escapes quote and angle-bracket chars in path', () => {
    const block: ContextBlock = {
      type: 'file',
      path: 'weird "name"<.ts>',
      content: 'x',
    };
    expect(toAnthropicXml(block)).toContain(
      'path="weird &quot;name&quot;&lt;.ts&gt;"',
    );
  });

  it('toOpenAiFunctionContext wraps with function-call-style delimiters', () => {
    const block: ContextBlock = {
      type: 'directory',
      path: 'tests/payments',
      content: 'a.ts\nb.ts',
    };
    expect(toOpenAiFunctionContext(block)).toBe(
      `[context(type="directory", path="tests/payments")]\na.ts\nb.ts\n[/context]`,
    );
  });

  it('toOpenAiFunctionContext omits path for pointer-style blocks', () => {
    const block: ContextBlock = { type: 'skill', content: 'ref:typescript-review' };
    expect(toOpenAiFunctionContext(block)).toBe(
      `[context(type="skill")]\nref:typescript-review\n[/context]`,
    );
  });
});

describe('context-adapter — providerFlavour', () => {
  it('maps known Anthropic ids', () => {
    expect(providerFlavour('anthropic' as ProviderId)).toBe('anthropic');
    expect(providerFlavour('claude-3-5' as ProviderId)).toBe('anthropic');
  });

  it('maps known OpenAI and compatible ids', () => {
    expect(providerFlavour('openai' as ProviderId)).toBe('openai');
    expect(providerFlavour('gpt-4o' as ProviderId)).toBe('openai');
    expect(providerFlavour('groq' as ProviderId)).toBe('openai');
    expect(providerFlavour('local-openai' as ProviderId)).toBe('openai');
    expect(providerFlavour('deepseek' as ProviderId)).toBe('openai');
  });

  it('falls back to anthropic for null / unknown ids', () => {
    expect(providerFlavour(null)).toBe('anthropic');
    expect(providerFlavour(undefined)).toBe('anthropic');
    expect(providerFlavour('some-proprietary-model' as ProviderId)).toBe(
      'anthropic',
    );
  });
});

describe('context-adapter — adaptContextBlocks', () => {
  const blocks: ContextBlock[] = [
    { type: 'file', path: 'src/app.ts', content: 'body-a' },
    { type: 'directory', path: 'tests', content: 'tests/a.ts\ntests/b.ts' },
  ];

  it('returns empty string for empty blocks', () => {
    expect(adaptContextBlocks([], 'anthropic')).toBe('');
    expect(adaptContextBlocks([], 'openai')).toBe('');
  });

  it('Anthropic: XML tags joined with newlines', () => {
    const out = adaptContextBlocks(blocks, 'anthropic');
    expect(out).toContain('<context type="file" path="src/app.ts">');
    expect(out).toContain('<context type="directory" path="tests">');
    expect(out.split('</context>').length).toBe(3); // 2 closes + trailing
  });

  it('OpenAI: function-call-style blocks joined with newlines', () => {
    const out = adaptContextBlocks(blocks, 'openai');
    expect(out).toContain('[context(type="file", path="src/app.ts")]');
    expect(out).toContain('[context(type="directory", path="tests")]');
    expect(out).toContain('[/context]');
    expect(out.indexOf('<context')).toBe(-1);
  });

  it('accepts a raw ProviderId and resolves flavour', () => {
    const a = adaptContextBlocks(blocks, 'anthropic' as ProviderId);
    const o = adaptContextBlocks(blocks, 'gpt-4o' as ProviderId);
    expect(a).toContain('<context');
    expect(o).toContain('[context(');
  });
});

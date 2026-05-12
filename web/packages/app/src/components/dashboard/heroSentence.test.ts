import { describe, expect, it } from 'vitest';
import { heroSentence, type HeroState } from './heroSentence';
import type { ProviderEntry, SessionSummary } from '../../ipc/dashboard';

function session(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: 'sess_0001',
    subject: 'sample session',
    state: 'active',
    persistence: 'persist',
    createdAt: '2026-04-18T00:00:00Z',
    lastEventAt: '2026-04-18T00:00:00Z',
    ...overrides,
  };
}

function provider(overrides: Partial<ProviderEntry> = {}): ProviderEntry {
  return {
    id: 'anthropic',
    display_name: 'Anthropic',
    credential_required: true,
    has_credential: true,
    model_available: true,
    ...overrides,
  };
}

function state(overrides: Partial<HeroState> = {}): HeroState {
  return {
    sessions: [],
    providers: [],
    credentialsPresent: {},
    ...overrides,
  };
}

describe('heroSentence (F-728)', () => {
  it('returns the verbatim empty-state copy when nothing is configured', () => {
    expect(heroSentence(state())).toBe('Ready to forge. Add a provider and start a session.');
  });

  it('returns the providers-but-no-sessions copy when at least one provider is ready', () => {
    const result = heroSentence(
      state({
        providers: [provider({ id: 'anthropic', display_name: 'Anthropic' })],
        credentialsPresent: { anthropic: true },
      }),
    );
    expect(result).toBe('Anthropic connected.');
  });

  it('returns the no-connected-providers copy when every provider is awaiting credentials', () => {
    const result = heroSentence(
      state({
        providers: [
          provider({
            id: 'anthropic',
            display_name: 'Anthropic',
            credential_required: true,
            has_credential: false,
          }),
        ],
        credentialsPresent: { anthropic: false },
      }),
    );
    expect(result).toBe('Ready when you are. Start a session above.');
  });

  it('renders the verbatim DoD example for the active state', () => {
    const result = heroSentence(
      state({
        sessions: [
          session({ id: 's1' }),
          session({ id: 's2' }),
          // The awaiting-approval session counts as both active and awaiting
          // per the wire shape — F-740 will add a distinct state; until
          // then we exercise the structural awaiting check.
          session({ id: 's3', state: 'awaiting approval' as SessionSummary['state'] }),
        ],
        providers: [
          provider({
            id: 'anthropic',
            display_name: 'Anthropic',
            credential_required: true,
            has_credential: true,
            model_available: true,
          }),
          provider({
            id: 'custom_openai:ollama',
            display_name: 'custom_openai — ollama',
            credential_required: true,
            has_credential: true,
            model_available: true,
          }),
          provider({
            id: 'openai',
            display_name: 'OpenAI',
            credential_required: true,
            has_credential: false,
            model_available: true,
          }),
        ],
        credentialsPresent: { anthropic: true, 'custom_openai:ollama': true, openai: false },
      }),
    );
    expect(result).toBe(
      'Two sessions active. One agent paused awaiting approval. Anthropic and custom_openai — ollama connected — OpenAI awaiting credentials.',
    );
  });

  it('uses the singular noun forms for single-count totals', () => {
    const result = heroSentence(
      state({
        sessions: [session({ id: 's1' })],
        providers: [provider({ id: 'anthropic', display_name: 'Anthropic' })],
        credentialsPresent: { anthropic: true },
      }),
    );
    expect(result).toBe('One session active. Anthropic connected.');
  });

  it('omits the awaiting-approval fragment when no agents are paused', () => {
    const result = heroSentence(
      state({
        sessions: [session({ id: 's1' }), session({ id: 's2' })],
        providers: [
          provider({
            id: 'anthropic',
            display_name: 'Anthropic',
            credential_required: true,
            has_credential: true,
          }),
        ],
        credentialsPresent: { anthropic: true },
      }),
    );
    expect(result).toBe('Two sessions active. Anthropic connected.');
  });

  it('omits the connected fragment when every provider is awaiting credentials', () => {
    const result = heroSentence(
      state({
        sessions: [session({ id: 's1' })],
        providers: [
          provider({
            id: 'anthropic',
            display_name: 'Anthropic',
            credential_required: true,
            has_credential: false,
          }),
        ],
        credentialsPresent: { anthropic: false },
      }),
    );
    expect(result).toBe('One session active. Anthropic awaiting credentials.');
  });

  it('renders digits for counts of ten or more', () => {
    const sessions = Array.from({ length: 12 }, (_, i) => session({ id: `s${i}` }));
    const result = heroSentence(
      state({
        sessions,
        providers: [provider({ id: 'anthropic', display_name: 'Anthropic' })],
        credentialsPresent: { anthropic: true },
      }),
    );
    expect(result).toBe('12 sessions active. Anthropic connected.');
  });

  it('serializes three connected providers with a comma and `and`', () => {
    const result = heroSentence(
      state({
        providers: [
          provider({
            id: 'anthropic',
            display_name: 'Anthropic',
            credential_required: true,
            has_credential: true,
          }),
          provider({
            id: 'openai',
            display_name: 'OpenAI',
            credential_required: true,
            has_credential: true,
          }),
          provider({ id: 'mistral', display_name: 'Mistral', credential_required: false }),
        ],
        credentialsPresent: { anthropic: true, openai: true },
      }),
    );
    expect(result).toBe('Anthropic, OpenAI and Mistral connected.');
  });

  it('treats a missing credentialsPresent entry as no credential', () => {
    const result = heroSentence(
      state({
        sessions: [session({ id: 's1' })],
        providers: [
          provider({
            id: 'anthropic',
            display_name: 'Anthropic',
            credential_required: true,
            has_credential: false,
          }),
        ],
        // credentialsPresent intentionally omits `anthropic`.
        credentialsPresent: {},
      }),
    );
    expect(result).toBe('One session active. Anthropic awaiting credentials.');
  });

  it('treats unconfigured (no model_available) providers as neither connected nor missing-credentials', () => {
    const result = heroSentence(
      state({
        sessions: [session({ id: 's1' })],
        providers: [
          provider({
            id: 'anthropic',
            display_name: 'Anthropic',
            credential_required: false,
            model_available: false,
          }),
        ],
      }),
    );
    expect(result).toBe('One session active.');
  });
});

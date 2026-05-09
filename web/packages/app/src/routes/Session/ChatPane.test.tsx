import { afterEach, describe, expect, it, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@solidjs/testing-library';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import type { SessionId } from '@forge/ipc';

// --- Store imports ---
import {
  pushEvent,
  setAwaitingResponse,
  resetMessagesStore,
} from '../../stores/messages';
import { setActiveSessionId } from '../../stores/session';
import { resetApprovalsStore } from '../../stores/approvals';
import { setInvokeForTesting } from '../../lib/tauri';
import { ChatPane, Composer, MAX_COMPOSER_BYTES, removeAtSpan } from './ChatPane';

const SID = 'session-chat-test' as SessionId;
const invokeMock = vi.fn();

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  setInvokeForTesting(invokeMock as never);
  resetMessagesStore();
  resetApprovalsStore();
  setActiveSessionId(SID);
});

afterEach(() => {
  setInvokeForTesting(null);
});

describe('ChatPane rendering', () => {
  it('renders the message list container', () => {
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('message-list')).toBeInTheDocument();
  });

  it('renders the composer textarea', () => {
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('composer-textarea')).toBeInTheDocument();
  });

  // F-415: fresh-session mount renders the empty-state placeholder inside
  // the message list so the composer isn't floating alone in a blank pane.
  // Copy matches the canonical `// noun-phrase` form from voice-terminology
  // §8 / ai-patterns §"Interaction states".
  it('renders the fresh-session empty-state placeholder when no turns exist', () => {
    const { getByTestId } = render(() => <ChatPane />);
    const placeholder = getByTestId('chat-pane-empty-state');
    expect(placeholder).toBeInTheDocument();
    expect(placeholder).toHaveTextContent('// composer ready');
  });

  it('hides the empty-state placeholder once the first turn arrives', () => {
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    expect(getByTestId('chat-pane-empty-state')).toBeInTheDocument();
    pushEvent(SID, { kind: 'UserMessage', text: 'First!', message_id: 'u0' });
    expect(queryByTestId('chat-pane-empty-state')).not.toBeInTheDocument();
  });

  it('hides the empty-state placeholder while awaiting a response', () => {
    setAwaitingResponse(SID, true);
    const { queryByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('chat-pane-empty-state')).not.toBeInTheDocument();
  });

  it('renders a user message turn', () => {
    pushEvent(SID, { kind: 'UserMessage', text: 'Hello world', message_id: 'u1' });
    const { getByText } = render(() => <ChatPane />);
    expect(getByText('Hello world')).toBeInTheDocument();
  });

  it('renders an assistant message turn', () => {
    pushEvent(SID, { kind: 'AssistantMessage', text: 'Hi there', message_id: 'a1' });
    const { getByText } = render(() => <ChatPane />);
    expect(getByText('Hi there')).toBeInTheDocument();
  });

  it('renders a streaming assistant turn with blinking cursor', () => {
    pushEvent(SID, { kind: 'AssistantDelta', delta: 'Typing...', message_id: 'a2' });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('streaming-cursor')).toBeInTheDocument();
  });

  it('does not render streaming cursor on completed messages', () => {
    pushEvent(SID, { kind: 'AssistantMessage', text: 'Done', message_id: 'a3' });
    const { queryByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('streaming-cursor')).not.toBeInTheDocument();
  });

  // F-405: assistive tech suppresses partial announcements when the
  // in-progress message carries aria-busy="true". The attribute must be
  // present only while the turn is streaming, then drop when it finalises.
  it('sets aria-busy="true" on a streaming assistant bubble', () => {
    pushEvent(SID, { kind: 'AssistantDelta', delta: 'Typing...', message_id: 'a-busy-1' });
    const { getByTestId } = render(() => <ChatPane />);
    const cursor = getByTestId('streaming-cursor');
    const bubble = cursor.closest('article');
    expect(bubble).not.toBeNull();
    expect(bubble).toHaveAttribute('aria-busy', 'true');
  });

  it('does not set aria-busy on a finalised assistant bubble', () => {
    pushEvent(SID, { kind: 'AssistantMessage', text: 'Done streaming', message_id: 'a-busy-2' });
    const { getByText } = render(() => <ChatPane />);
    const bubble = getByText('Done streaming').closest('article');
    expect(bubble).not.toBeNull();
    expect(bubble).not.toHaveAttribute('aria-busy');
  });

  it('aria-busy cycles on the same turn: present while streaming, gone once finalised', () => {
    pushEvent(SID, { kind: 'AssistantDelta', delta: 'Hello', message_id: 'a-busy-3' });
    const { container } = render(() => <ChatPane />);
    const streamingBubble = container.querySelector('article.turn--assistant');
    expect(streamingBubble).not.toBeNull();
    expect(streamingBubble).toHaveAttribute('aria-busy', 'true');

    pushEvent(SID, { kind: 'AssistantMessage', text: 'Hello world', message_id: 'a-busy-3' });
    const finalisedBubble = container.querySelector('article.turn--assistant');
    expect(finalisedBubble).not.toBeNull();
    expect(finalisedBubble).not.toHaveAttribute('aria-busy');
  });

  it('renders a tool call card', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-1',
      tool_name: 'fs.read',
      args_json: '{}',
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-card-tc-1')).toBeInTheDocument();
  });

  // F-041: collapsed tool call card renders a one-line arg summary to the
  // right of the tool name. Path-taking tools show the path; other tools
  // show a short stringified JSON; unparseable args render no summary.
  it('renders the path as the arg summary when args_json has a path field', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-path',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'readable.txt' }),
    });
    const { getByTestId } = render(() => <ChatPane />);
    const card = getByTestId('tool-call-card-tc-path');
    expect(card).toHaveTextContent('readable.txt');
  });

  it('falls back to stringified args truncated to ~60 chars when no path field', () => {
    const bigArgs = { query: 'x'.repeat(120), scope: 'everything' };
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-nopath',
      tool_name: 'search',
      args_json: JSON.stringify(bigArgs),
    });
    const { getByTestId } = render(() => <ChatPane />);
    const summary = getByTestId('tool-call-args-tc-nopath');
    // Contains the leading tokens of the stringified JSON…
    expect(summary).toHaveTextContent('"query"');
    // …but is bounded by the ~60-char cap, not the full 100+ char payload.
    // F-447: the expanded body renders the full pretty-printed args — so
    // the truncation assertion must scope to the collapsed summary span.
    expect(summary.textContent ?? '').not.toContain('x'.repeat(80));
  });

  // F-080 item 6: the prior "unparseable args_json" test was deleted because
  // `args_json` is produced exclusively by `fromRustEvent`
  // (`JSON.stringify(ev['args'] ?? null)`) and is contractually valid JSON at
  // the ChatPane boundary. The defensive `try/catch` blocks that handled
  // unparseable input were removed in F-080; pinning this test would have
  // required keeping a dead code path in the component just to satisfy an
  // unreachable input shape.

  it('renders an error turn inline', () => {
    pushEvent(SID, { kind: 'Error', message: 'ECONNREFUSED 127.0.0.1:11434' });
    const { getByText } = render(() => <ChatPane />);
    expect(getByText(/ECONNREFUSED/)).toBeInTheDocument();
  });
});

describe('Composer keyboard behavior (Option B)', () => {
  it('sends message on Enter key', async () => {
    invokeMock.mockResolvedValue(undefined);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.input(textarea, { target: { value: 'send this' } });
    fireEvent.keyDown(textarea, { key: 'Enter', code: 'Enter', shiftKey: false, ctrlKey: false, metaKey: false });

    expect(invokeMock).toHaveBeenCalledWith('session_send_message', {
      sessionId: SID,
      text: 'send this',
    });
  });

  it('inserts newline on Shift+Enter instead of sending', () => {
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.input(textarea, { target: { value: 'line1' } });
    fireEvent.keyDown(textarea, { key: 'Enter', code: 'Enter', shiftKey: true });

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it('inserts newline on Ctrl+Enter instead of sending', () => {
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.input(textarea, { target: { value: 'line1' } });
    fireEvent.keyDown(textarea, { key: 'Enter', code: 'Enter', ctrlKey: true });

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it('inserts newline on Cmd+Enter instead of sending', () => {
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.input(textarea, { target: { value: 'line1' } });
    fireEvent.keyDown(textarea, { key: 'Enter', code: 'Enter', metaKey: true });

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it('clears the textarea after sending', async () => {
    invokeMock.mockResolvedValue(undefined);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.input(textarea, { target: { value: 'hello' } });
    fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: false, ctrlKey: false, metaKey: false });

    expect(textarea.value).toBe('');
  });

  it('does not send an empty message', () => {
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: false, ctrlKey: false, metaKey: false });

    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe('Composer disabled state', () => {
  it('disables the textarea while awaiting response', () => {
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    expect(textarea).toBeDisabled();
  });

  it('enables the textarea when not awaiting', () => {
    setAwaitingResponse(SID, false);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    expect(textarea).not.toBeDisabled();
  });

  // F-040: the AssistantDelta handler clears `awaitingResponse` to false on the
  // first token, so a predicate based only on awaitingResponse re-enables the
  // composer mid-stream. The composer must stay disabled until the stream
  // finalises (streamingMessageId !== null), then re-enable on the final.
  it('stays disabled across awaiting → delta → final, then re-enables', () => {
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    expect(textarea).toBeDisabled();

    // First delta arrives — `awaitingResponse` flips to false in the store,
    // but `streamingMessageId` is set; composer must STILL be disabled.
    pushEvent(SID, { kind: 'AssistantDelta', delta: 'Hi', message_id: 'turn-1' });
    expect(textarea).toBeDisabled();

    // Mid-stream delta — still disabled.
    pushEvent(SID, { kind: 'AssistantDelta', delta: ' there.', message_id: 'turn-1' });
    expect(textarea).toBeDisabled();

    // Stream finalises — composer re-enables.
    pushEvent(SID, { kind: 'AssistantMessage', text: 'Hi there.', message_id: 'turn-1' });
    expect(textarea).not.toBeDisabled();
  });

  it('shows a streaming indicator while awaiting response', () => {
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('streaming-indicator')).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// F-086: composer disabled-state CSS contract
// ---------------------------------------------------------------------------
//
// `component-principles.md` ("Buttons"): "Disabled buttons use iron-600
// background and text. Never reduce opacity on a button to show disabled
// state — opacity makes elements appear interactive." The rule applies to
// any interactive control, including the composer textarea while a stream
// is locked. jsdom does not resolve external stylesheets at render time, so
// the CSS source itself is the contract — assert against the source the
// way `check-tokens.test.ts` asserts against `tokens.css`.
describe('Composer disabled state CSS (F-086)', () => {
  const cssSource = readFileSync(
    resolve(__dirname, 'ChatPane.css'),
    'utf-8',
  );

  // Match `.composer__textarea:disabled { ... }` and capture the rule body.
  const ruleMatch = cssSource.match(
    /\.composer__textarea:disabled\s*\{([^}]*)\}/,
  );

  it('declares a `:disabled` rule for the composer textarea', () => {
    expect(ruleMatch).not.toBeNull();
  });

  it('does not reduce opacity to signal disabled (component-principles.md)', () => {
    const body = ruleMatch?.[1] ?? '';
    expect(body).not.toMatch(/\bopacity\s*:/);
  });

  it('uses --color-surface-2 for the locked background', () => {
    const body = ruleMatch?.[1] ?? '';
    expect(body).toMatch(/background\s*:\s*var\(--color-surface-2\)/);
  });

  it('uses --color-text-disabled for the locked text color', () => {
    const body = ruleMatch?.[1] ?? '';
    expect(body).toMatch(/color\s*:\s*var\(--color-text-disabled\)/);
  });

  it('keeps `cursor: not-allowed` to signal the locked state', () => {
    const body = ruleMatch?.[1] ?? '';
    expect(body).toMatch(/cursor\s*:\s*not-allowed/);
  });
});

describe('Auto-scroll behavior', () => {
  it('message list has data-autoscroll attribute for pinning', () => {
    const { getByTestId } = render(() => <ChatPane />);
    const list = getByTestId('message-list');
    expect(list).toHaveAttribute('data-autoscroll');
  });

  it('scrolls to bottom when a new streaming delta arrives', () => {
    const { getByTestId } = render(() => <ChatPane />);
    const list = getByTestId('message-list');

    // Mock scrollTop/scrollHeight so we can verify scrollTop was set
    Object.defineProperty(list, 'scrollHeight', { value: 500, configurable: true });
    Object.defineProperty(list, 'clientHeight', { value: 200, configurable: true });

    pushEvent(SID, { kind: 'AssistantDelta', delta: 'streaming text', message_id: 'stream-1' });

    // After a delta, the list should be pinned to bottom (scrollTop === scrollHeight)
    expect(list.scrollTop).toBe(500);
  });
});

// ---------------------------------------------------------------------------
// Inline approval prompt (F-027)
// ---------------------------------------------------------------------------

describe('Inline approval prompt', () => {
  const PREVIEW = { description: 'Edit file /src/foo.ts: 3 hunks, +47 -21' };
  const FS_EDIT_ARGS = JSON.stringify({ path: '/src/foo.ts', patch: '...' });

  it('renders the approval prompt when ToolCallApprovalRequested arrives', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-ap-1',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('approval-prompt')).toBeInTheDocument();
  });

  it('displays preview description inside the prompt', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-ap-2',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('approval-preview')).toHaveTextContent('Edit file /src/foo.ts');
  });

  it('invokes session_approve_tool with Once when approve is clicked', async () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-ap-3',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('approve-once-btn'));
    expect(invokeMock).toHaveBeenCalledWith('session_approve_tool', {
      sessionId: SID,
      toolCallId: 'tc-ap-3',
      scope: 'Once',
    });
  });

  it('invokes session_reject_tool when reject is clicked', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-ap-4',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('reject-btn'));
    expect(invokeMock).toHaveBeenCalledWith('session_reject_tool', {
      sessionId: SID,
      toolCallId: 'tc-ap-4',
    });
  });

  it('invokes session_approve_tool with ThisFile from scope menu', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-ap-5',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-file-btn'));
    expect(invokeMock).toHaveBeenCalledWith('session_approve_tool', {
      sessionId: SID,
      toolCallId: 'tc-ap-5',
      scope: 'ThisFile',
    });
  });

  it('invokes session_approve_tool with ThisTool from scope menu', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-ap-6',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-tool-btn'));
    expect(invokeMock).toHaveBeenCalledWith('session_approve_tool', {
      sessionId: SID,
      toolCallId: 'tc-ap-6',
      scope: 'ThisTool',
    });
  });
});

// ---------------------------------------------------------------------------
// Whitelist auto-approve + pill rendering (F-027)
// ---------------------------------------------------------------------------

describe('Whitelist auto-approve', () => {
  const PREVIEW = { description: 'Edit file /src/foo.ts' };
  const FS_EDIT_ARGS = JSON.stringify({ path: '/src/foo.ts', patch: '...' });

  it('shows whitelisted pill when ThisFile scope was granted for same path', async () => {
    // First render — approve ThisFile scope
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-wl-1',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId, unmount } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-file-btn'));
    unmount();

    // Reset message store but keep approvals store so whitelist persists
    resetMessagesStore();

    // Second call — same tool + path, should show whitelisted pill
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-wl-2',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getAllByTestId } = render(() => <ChatPane />);
    const pills = getAllByTestId('whitelisted-pill');
    expect(pills).toHaveLength(1);
    expect(pills[0]).toHaveTextContent('whitelisted · this file');
  });

  it('auto-invokes session_approve_tool for whitelisted match', async () => {
    // First render — approve ThisTool scope
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-wl-auto-1',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId, unmount } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-tool-btn'));
    unmount();

    // Reset messages but keep approvals whitelist
    resetMessagesStore();
    invokeMock.mockClear();

    // Second call — same tool, auto-approve effect fires on mount
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-wl-auto-2',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    render(() => <ChatPane />);
    expect(invokeMock).toHaveBeenCalledWith('session_approve_tool', expect.objectContaining({
      toolCallId: 'tc-wl-auto-2',
      scope: 'ThisTool',
    }));
  });

  it('hides approval prompt and shows pill when auto-approved', async () => {
    // First render — approve ThisFile scope
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-wl-hide-1',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId, unmount } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-file-btn'));
    unmount();

    // Reset messages but keep approvals whitelist
    resetMessagesStore();

    // Second call — pill shown, no approval-prompt
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-wl-hide-2',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { queryByTestId, getAllByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('approval-prompt')).not.toBeInTheDocument();
    const pills = getAllByTestId('whitelisted-pill');
    expect(pills).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// F-079: error handling on invoke rejection
// ---------------------------------------------------------------------------

async function flushMicrotasks(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

describe('invoke rejection handling (F-079)', () => {
  it('clears awaitingResponse and surfaces an error turn when session_send_message rejects', async () => {
    invokeMock.mockReset();
    invokeMock.mockRejectedValueOnce(new Error('send failed'));

    const { getByTestId, findByText } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.input(textarea, { target: { value: 'hello' } });
    fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: false, ctrlKey: false, metaKey: false });

    // Composer flips to disabled synchronously when handleSend sets awaitingResponse(true).
    expect(textarea).toBeDisabled();

    // After the rejected invoke promise resolves, awaitingResponse must be cleared
    // (composer re-enabled) AND an error turn must surface the failure.
    await flushMicrotasks();
    expect(textarea).not.toBeDisabled();
    expect(await findByText(/send failed/)).toBeInTheDocument();
  });

  it('surfaces an error turn when session_approve_tool rejects from the inline prompt', async () => {
    invokeMock.mockReset();
    invokeMock.mockRejectedValueOnce(new Error('approve boom'));

    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-rej-approve',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/x.ts', patch: '...' }),
      preview: { description: 'Edit /src/x.ts' },
    });

    const { getByTestId, findByText } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('approve-once-btn'));

    await flushMicrotasks();
    expect(await findByText(/approve boom/)).toBeInTheDocument();
  });

  it('surfaces an error turn when session_reject_tool rejects', async () => {
    invokeMock.mockReset();
    invokeMock.mockRejectedValueOnce(new Error('reject boom'));

    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-rej-reject',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/x.ts', patch: '...' }),
      preview: { description: 'Edit /src/x.ts' },
    });

    const { getByTestId, findByText } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('reject-btn'));

    await flushMicrotasks();
    expect(await findByText(/reject boom/)).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// F-080 item 5: composer message-byte cap (defense-in-depth)
// ---------------------------------------------------------------------------
//
// The Rust side enforces a 128 KiB cap; the composer caps at 100 KiB so the
// user gets immediate feedback without an IPC round trip. Both the warning
// surface and the send-blocking behavior are user-facing contracts.
describe('Composer message-byte cap (F-080)', () => {
  it('does not show the overflow warning under the cap', () => {
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    fireEvent.input(textarea, { target: { value: 'hello' } });
    expect(queryByTestId('composer-overflow-warning')).not.toBeInTheDocument();
  });

  it('shows the inline overflow warning when the trimmed payload exceeds the cap', () => {
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    const overCap = 'a'.repeat(MAX_COMPOSER_BYTES + 1);
    fireEvent.input(textarea, { target: { value: overCap } });
    expect(getByTestId('composer-overflow-warning')).toBeInTheDocument();
  });

  it('blocks send on Enter when over the byte cap', () => {
    invokeMock.mockReset();
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    const overCap = 'a'.repeat(MAX_COMPOSER_BYTES + 1);
    fireEvent.input(textarea, { target: { value: overCap } });
    fireEvent.keyDown(textarea, {
      key: 'Enter',
      code: 'Enter',
      shiftKey: false,
      ctrlKey: false,
      metaKey: false,
    });
    // No `session_send_message` invocation should have been made — the
    // composer-side cap intercepts the send before it reaches the bridge.
    const sendCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === 'session_send_message',
    );
    expect(sendCalls).toHaveLength(0);
  });

  it('counts UTF-8 bytes, not UTF-16 code units (multi-byte characters)', () => {
    // Each "💥" is 4 UTF-8 bytes but only 2 UTF-16 code units. Picking a count
    // that is over the cap in bytes but well under it in `String.length` units
    // proves the implementation uses TextEncoder rather than `.length`.
    const emoji = '💥';
    const utf8PerChar = new TextEncoder().encode(emoji).length; // 4
    const overCount = Math.ceil(MAX_COMPOSER_BYTES / utf8PerChar) + 1;
    const value = emoji.repeat(overCount);
    // Sanity-check the corner: byte length over cap, code-unit length under.
    expect(new TextEncoder().encode(value).length).toBeGreaterThan(MAX_COMPOSER_BYTES);

    invokeMock.mockReset();
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    fireEvent.input(textarea, { target: { value } });
    expect(getByTestId('composer-overflow-warning')).toBeInTheDocument();
    fireEvent.keyDown(textarea, {
      key: 'Enter',
      code: 'Enter',
      shiftKey: false,
      ctrlKey: false,
      metaKey: false,
    });
    expect(
      invokeMock.mock.calls.filter((c) => c[0] === 'session_send_message'),
    ).toHaveLength(0);
  });

  it('still sends a payload exactly at the cap', () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    const atCap = 'a'.repeat(MAX_COMPOSER_BYTES);
    fireEvent.input(textarea, { target: { value: atCap } });
    fireEvent.keyDown(textarea, {
      key: 'Enter',
      code: 'Enter',
      shiftKey: false,
      ctrlKey: false,
      metaKey: false,
    });
    expect(invokeMock).toHaveBeenCalledWith(
      'session_send_message',
      expect.objectContaining({ sessionId: SID, text: atCap }),
    );
  });
});

// ---------------------------------------------------------------------------
// F-080 item 6: args_json boundary contract — `fromRustEvent` always
// produces valid JSON, so removing the defensive try/catches in ChatPane
// must not regress the path-extraction or rendering behavior.
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// F-141: @-trigger + ContextPicker integration in the composer
// ---------------------------------------------------------------------------
//
// The DoD requires: typing `@` opens the picker; selecting a result appends
// a ContextChip to the `ctx-chips` row above the textarea AND removes the
// `@text` span; Escape dismisses without inserting. Full keyboard coverage
// for the picker itself lives in ContextPicker.test.tsx — these tests pin
// the *composer-side* plumbing (trigger detection, chip row population,
// textarea mutation on insert).
describe('Composer @-trigger and ContextPicker integration (F-141)', () => {
  it('does not render the picker while the textarea holds no `@` token', () => {
    const { queryByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('context-picker')).not.toBeInTheDocument();
  });

  it('opens the picker when the user types `@`', () => {
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    // Typing `@` at the start of a fresh composer — the simplest trigger.
    textarea.value = '@';
    textarea.selectionStart = 1;
    textarea.selectionEnd = 1;
    fireEvent.input(textarea);
    expect(queryByTestId('context-picker')).toBeInTheDocument();
  });

  it('closes the picker once the user types a space after `@`', () => {
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    textarea.value = '@foo';
    textarea.selectionStart = 4;
    textarea.selectionEnd = 4;
    fireEvent.input(textarea);
    expect(queryByTestId('context-picker')).toBeInTheDocument();
    textarea.value = '@foo ';
    textarea.selectionStart = 5;
    textarea.selectionEnd = 5;
    fireEvent.input(textarea);
    expect(queryByTestId('context-picker')).not.toBeInTheDocument();
  });

  it('does not send the message when Enter is pressed while the picker is open', () => {
    invokeMock.mockReset();
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    textarea.value = '@foo';
    textarea.selectionStart = 4;
    textarea.selectionEnd = 4;
    fireEvent.input(textarea);
    fireEvent.keyDown(textarea, { key: 'Enter' });
    const sendCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === 'session_send_message',
    );
    expect(sendCalls).toHaveLength(0);
  });

  it('renders the ctx-chips row above the textarea', () => {
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('ctx-chips')).toBeInTheDocument();
  });

  // DoD item 4: "selected result appends a `ContextChip` to `ctx-chips` row;
  // the `@text` span is replaced". `removeAtSpan` is the pure-function half
  // of that (the span removal + caret positioning). Unit-testing it directly
  // decouples the invariant from the picker's async onPick wiring.
  it('removeAtSpan removes an @token at the start of text and keeps the caret at 0', () => {
    const result = removeAtSpan('@foo', 0, 4);
    expect(result.text).toBe('');
    expect(result.caret).toBe(0);
  });

  it('removeAtSpan removes an @token embedded in text and positions the caret at the join', () => {
    const text = 'hello @foo world';
    // caret sits just after "foo" — span is "@foo".
    const result = removeAtSpan(text, 6, 10);
    expect(result.text).toBe('hello  world');
    expect(result.caret).toBe(6);
  });

  it('removeAtSpan preserves trailing text that was after the caret', () => {
    const text = '@partial rest';
    const result = removeAtSpan(text, 0, 8);
    expect(result.text).toBe(' rest');
    expect(result.caret).toBe(0);
  });

  // DoD item 4 end-to-end: render the Composer with a seeded category, open
  // the picker via `@`, pick a result, and assert the chip lands in the
  // ctx-chips row AND the `@text` span is removed from the textarea.
  it('end-to-end: @-trigger → pick result → chip appears in ctx-chips, @text span removed', () => {
    const items = {
      file: [
        { category: 'file' as const, label: 'alpha.ts', value: 'src/alpha.ts' },
      ],
    };
    const { getByTestId, queryByTestId } = render(() => (
      <Composer disabled={false} onSend={() => {}} items={items} />
    ));
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    textarea.value = 'pref @foo';
    textarea.selectionStart = 9;
    textarea.selectionEnd = 9;
    fireEvent.input(textarea);
    // Picker is open with the seeded file result.
    expect(queryByTestId('context-picker')).toBeInTheDocument();
    expect(queryByTestId('context-picker-result-0')).toBeInTheDocument();
    // Click the first result.
    fireEvent.mouseDown(getByTestId('context-picker-result-0'));
    // Chip landed in the ctx-chips row with the picked label.
    const chip = getByTestId('ctx-chip');
    expect(chip).toHaveTextContent('alpha.ts');
    // Picker closed.
    expect(queryByTestId('context-picker')).not.toBeInTheDocument();
    // `@text` span removed from the textarea.
    expect(textarea.value).toBe('pref ');
  });

  it('Escape while picker is open dismisses without inserting a chip', () => {
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    textarea.value = '@foo';
    textarea.selectionStart = 4;
    textarea.selectionEnd = 4;
    fireEvent.input(textarea);
    expect(queryByTestId('context-picker')).toBeInTheDocument();
    fireEvent.keyDown(window, { key: 'Escape' });
    // Picker must close, no chip inserted (chips row stays empty).
    expect(queryByTestId('context-picker')).not.toBeInTheDocument();
    expect(queryByTestId('ctx-chip')).not.toBeInTheDocument();
  });

  // F-142 DoD item 4: ChatPane wires the resolver registry + provider adapter
  // into the send path — a file chip emits a prepended `<context ...>` block
  // above the user text on the IPC call. Injects a stub registry so the test
  // doesn't need a running Rust backend.
  it('F-142 integration: chip at send time prepends an Anthropic-shaped block to text', async () => {
    const stubRegistry = {
      file: {
        list: async () => [
          { category: 'file' as const, label: 'app.ts', value: '/ws/app.ts' },
        ],
        resolve: async () => ({
          type: 'file' as const,
          path: '/ws/app.ts',
          content: 'body-of-app',
        }),
      },
    };
    const { getByTestId } = render(() => (
      <ChatPane registry={stubRegistry} providerId={'anthropic'} />
    ));
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    // Open the picker, pick the only file candidate, type a message, send.
    textarea.value = '@app';
    textarea.selectionStart = 4;
    textarea.selectionEnd = 4;
    fireEvent.input(textarea);
    // `list(query)` runs async; wait a tick for the picker items to populate.
    await new Promise((r) => setTimeout(r, 0));
    fireEvent.mouseDown(getByTestId('context-picker-result-0'));
    fireEvent.input(textarea, { target: { value: 'explain' } });
    fireEvent.keyDown(textarea, { key: 'Enter' });
    // resolveChips is async — wait a microtask for the prefix path.
    await new Promise((r) => setTimeout(r, 0));
    const call = invokeMock.mock.calls.find(
      (c) => c[0] === 'session_send_message',
    );
    expect(call).toBeDefined();
    const sentText = (call![1] as { text: string }).text;
    expect(sentText).toContain('<context type="file" path="/ws/app.ts">');
    expect(sentText).toContain('body-of-app');
    expect(sentText).toContain('explain');
    // The user text follows the context block, separated by a blank line.
    expect(sentText.trim().endsWith('explain')).toBe(true);
  });

  // F-142 DoD item 2: send flow routes chips through the resolver registry
  // and prepends a provider-shaped block to the user text. Uses a Composer
  // directly with a stub `onSend` so the assertion stays scoped to the
  // composer's chip-forwarding contract.
  it('F-142: onSend receives the currently-attached chips and clears them', () => {
    const items = {
      file: [
        { category: 'file' as const, label: 'app.ts', value: '/ws/app.ts' },
      ],
    };
    const onSend = vi.fn();
    const { getByTestId, queryByTestId } = render(() => (
      <Composer disabled={false} onSend={onSend} items={items} />
    ));
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    textarea.value = '@app';
    textarea.selectionStart = 4;
    textarea.selectionEnd = 4;
    fireEvent.input(textarea);
    fireEvent.mouseDown(getByTestId('context-picker-result-0'));
    // One chip is now attached.
    expect(getByTestId('ctx-chip')).toHaveTextContent('app.ts');
    // Type a message and send with Enter.
    fireEvent.input(textarea, { target: { value: 'please review' } });
    fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(onSend).toHaveBeenCalledWith(
      'please review',
      expect.arrayContaining([
        expect.objectContaining({
          category: 'file',
          label: 'app.ts',
          value: '/ws/app.ts',
        }),
      ]),
    );
    // Chips are consumed on send.
    expect(queryByTestId('ctx-chip')).not.toBeInTheDocument();
  });
});

describe('args_json boundary contract (F-080)', () => {
  it('renders the path and uses it for whitelist matching when args is a path-bearing object', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-bc-path',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: '/etc/hosts' }),
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-card-tc-bc-path')).toHaveTextContent('/etc/hosts');
  });

  it('falls back to the stringified payload for non-object args (boundary edge: "null")', () => {
    // `fromRustEvent` writes `JSON.stringify(ev['args'] ?? null)` so an absent
    // `args` field arrives here as the literal string "null". The summary
    // should still render that — no try/catch to swallow it.
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-bc-null',
      tool_name: 'noargs',
      args_json: 'null',
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-card-tc-bc-null')).toHaveTextContent('null');
  });
});

// ---------------------------------------------------------------------------
// F-036: persistent approvals — approve/revoke at workspace/user level
// ---------------------------------------------------------------------------

import { setActiveWorkspaceRoot } from '../../stores/session';
import {
  seedPersistentApprovals,
  getApprovalWhitelist,
} from '../../stores/approvals';

describe('ChatPane — persistent approvals (F-036)', () => {
  const PREVIEW = { description: 'Edit file /src/foo.ts' };
  const FS_EDIT_ARGS = JSON.stringify({ path: '/src/foo.ts', patch: '...' });

  beforeEach(() => {
    setActiveWorkspaceRoot('/ws');
  });

  afterEach(() => {
    setActiveWorkspaceRoot(null);
  });

  it('invokes save_approval with level=workspace for ThisFile + Workspace tier', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-ws',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('level-workspace-btn'));
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-file-btn'));

    const saveCall = invokeMock.mock.calls.find((c) => c[0] === 'save_approval');
    expect(saveCall).toBeDefined();
    expect(saveCall?.[1]).toMatchObject({
      level: 'workspace',
      workspaceRoot: '/ws',
      entry: expect.objectContaining({
        scope_key: 'file:fs.edit:/src/foo.ts',
        tool_name: 'fs.edit',
        label: 'this file',
      }),
    });
  });

  it('invokes save_approval with level=user for ThisTool + User tier', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-user',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('level-user-btn'));
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-tool-btn'));

    const saveCall = invokeMock.mock.calls.find((c) => c[0] === 'save_approval');
    expect(saveCall?.[1]).toMatchObject({
      level: 'user',
      workspaceRoot: '/ws',
      entry: expect.objectContaining({
        scope_key: 'tool:fs.edit',
        label: 'this tool',
      }),
    });
  });

  it('does NOT invoke save_approval for Session level', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-session',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-file-btn'));

    expect(
      invokeMock.mock.calls.filter((c) => c[0] === 'save_approval'),
    ).toHaveLength(0);
  });

  it('does NOT invoke save_approval for a one-shot Once approval even with Workspace selected', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-once-ws',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('level-workspace-btn'));
    fireEvent.click(getByTestId('approve-once-btn'));

    expect(
      invokeMock.mock.calls.filter((c) => c[0] === 'save_approval'),
    ).toHaveLength(0);
  });

  it('renders workspace-level pill with provenance suffix when seeded', () => {
    seedPersistentApprovals(SID, [
      {
        scope_key: 'file:fs.edit:/src/foo.ts',
        tool_name: 'fs.edit',
        label: 'this file',
        level: 'workspace',
      },
    ]);
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-pill-ws',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('whitelisted-pill')).toHaveTextContent(
      'whitelisted · this file · workspace',
    );
  });

  it('renders user-level pill with provenance suffix when seeded', () => {
    seedPersistentApprovals(SID, [
      {
        scope_key: 'tool:fs.edit',
        tool_name: 'fs.edit',
        label: 'this tool',
        level: 'user',
      },
    ]);
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-pill-user',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('whitelisted-pill')).toHaveTextContent(
      'whitelisted · this tool · user',
    );
  });

  it('invokes remove_approval when a workspace-level entry is revoked via the pill', () => {
    seedPersistentApprovals(SID, [
      {
        scope_key: 'tool:fs.edit',
        tool_name: 'fs.edit',
        label: 'this tool',
        level: 'workspace',
      },
    ]);
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-revoke-ws',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    invokeMock.mockClear();
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('whitelisted-pill'));
    fireEvent.click(getByTestId('revoke-btn'));

    const removeCall = invokeMock.mock.calls.find((c) => c[0] === 'remove_approval');
    expect(removeCall?.[1]).toEqual({
      scopeKey: 'tool:fs.edit',
      level: 'workspace',
      workspaceRoot: '/ws',
    });
    // The entry is also removed from the in-memory whitelist.
    expect('tool:fs.edit' in getApprovalWhitelist(SID).entries).toBe(false);
  });

  it('does NOT invoke remove_approval when a session-level entry is revoked', () => {
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-revoke-session-1',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const { getByTestId } = render(() => <ChatPane />);
    // Approve ThisFile at session level first.
    fireEvent.click(getByTestId('approve-dropdown-btn'));
    fireEvent.click(getByTestId('scope-file-btn'));

    // A second call with the same key — pill rendered, revoke.
    resetMessagesStore();
    invokeMock.mockClear();
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-f36-revoke-session-2',
      tool_name: 'fs.edit',
      args_json: FS_EDIT_ARGS,
      preview: PREVIEW,
    });
    const second = render(() => <ChatPane />);
    fireEvent.click(second.getByTestId('whitelisted-pill'));
    fireEvent.click(second.getByTestId('revoke-btn'));

    expect(
      invokeMock.mock.calls.filter((c) => c[0] === 'remove_approval'),
    ).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// F-136: SubAgentBanner inline mount via SubAgentSpawned store event
// ---------------------------------------------------------------------------
//
// The DoD requires ChatPane to listen for `SubAgentSpawned` events and mount
// a `SubAgentBanner` inline at the message position. The banner also flips
// running → done when a matching `BackgroundAgentCompleted` arrives (F-137
// already forwards that event onto the session bus).
describe('ChatPane — sub-agent banner inline mount (F-136)', () => {
  it('mounts a SubAgentBanner turn when SubAgentSpawned is pushed to the store', () => {
    pushEvent(SID, {
      kind: 'SubAgentSpawned',
      parent_instance_id: 'parent-inst-1',
      child_instance_id: 'child-inst-1',
      from_msg: 'msg-7',
      agent_name: 'test-writer',
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('sub-agent-banner-child-inst-1')).toBeInTheDocument();
    expect(
      getByTestId('sub-agent-banner-header-child-inst-1'),
    ).toHaveTextContent('test-writer');
  });

  it('renders the banner inline between surrounding turns in order of arrival', () => {
    pushEvent(SID, { kind: 'UserMessage', text: 'USER-MARKER', message_id: 'u-1' });
    pushEvent(SID, {
      kind: 'SubAgentSpawned',
      parent_instance_id: 'parent-inst-1',
      child_instance_id: 'child-inst-2',
      from_msg: 'u-1',
      agent_name: 'reviewer',
    });
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'ASSIST-MARKER',
      message_id: 'a-1',
    });
    const { getByTestId, getByText } = render(() => <ChatPane />);
    const list = getByTestId('message-list');
    const banner = getByTestId('sub-agent-banner-child-inst-2');
    expect(list).toContainElement(banner);
    // Order: user text precedes banner; assistant text follows it.
    // Markers are unique so `.textContent` matching doesn't cross-match
    // the banner's "spawned · reviewer" header.
    const nodes = Array.from(list.children);
    const userIdx = nodes.findIndex((n) => n.textContent?.includes('USER-MARKER'));
    const bannerIdx = nodes.findIndex((n) => n === banner || n.contains(banner));
    const assistantIdx = nodes.findIndex((n) =>
      n.textContent?.includes('ASSIST-MARKER'),
    );
    expect(userIdx).toBeGreaterThanOrEqual(0);
    expect(bannerIdx).toBeGreaterThan(userIdx);
    expect(assistantIdx).toBeGreaterThan(bannerIdx);
    // Sanity: the assistant text still rendered, so the banner didn't
    // swallow or replace subsequent turns.
    expect(getByText('ASSIST-MARKER')).toBeInTheDocument();
  });

  it('flips a banner from running → done when BackgroundAgentCompleted arrives for the same child id', () => {
    pushEvent(SID, {
      kind: 'SubAgentSpawned',
      parent_instance_id: 'parent-inst-1',
      child_instance_id: 'child-inst-3',
      from_msg: 'msg-x',
      agent_name: 'reviewer',
    });
    const { getByTestId } = render(() => <ChatPane />);
    const chip = getByTestId('sub-agent-banner-state-child-inst-3');
    expect(chip).toHaveAttribute('data-state', 'running');

    pushEvent(SID, {
      kind: 'BackgroundAgentCompleted',
      instance_id: 'child-inst-3',
    });
    expect(chip).toHaveAttribute('data-state', 'done');
  });

  it('is idempotent — a duplicate SubAgentSpawned for the same child does not stack banners', () => {
    pushEvent(SID, {
      kind: 'SubAgentSpawned',
      parent_instance_id: 'parent-inst-1',
      child_instance_id: 'child-inst-4',
      from_msg: 'msg-a',
    });
    pushEvent(SID, {
      kind: 'SubAgentSpawned',
      parent_instance_id: 'parent-inst-1',
      child_instance_id: 'child-inst-4',
      from_msg: 'msg-a',
    });
    const { getAllByTestId } = render(() => <ChatPane />);
    const banners = getAllByTestId('sub-agent-banner-child-inst-4');
    expect(banners).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// F-145 — branch-aware ChatPane rendering
// ---------------------------------------------------------------------------

describe('ChatPane — branch variants (F-145)', () => {
  const seedBranchedTurn = (): void => {
    // Initial user then branch root assistant.
    pushEvent(SID, { kind: 'UserMessage', text: 'ask', message_id: 'u1' });
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'root answer',
      message_id: 'root-1',
      branch_parent: null,
      branch_variant_index: 0,
      provider: 'mock',
      model: 'sonnet-4.5',
      at: '2026-04-20T14:22:11Z',
    });
    // Sibling variant.
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'variant one',
      message_id: 'var-1',
      branch_parent: 'root-1',
      branch_variant_index: 1,
      provider: 'mock',
      model: 'sonnet-4.5',
      at: '2026-04-20T14:24:35Z',
    });
  };

  it('renders no branch chrome for a single-variant assistant turn', () => {
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'lone',
      message_id: 'root-1',
      branch_parent: null,
      branch_variant_index: 0,
    });
    const { queryByTestId, getByText } = render(() => <ChatPane />);
    expect(getByText('lone')).toBeInTheDocument();
    expect(queryByTestId('branch-selector-strip')).toBeNull();
    expect(queryByTestId('branch-gutter')).toBeNull();
  });

  it('mounts the strip + gutter around a branched assistant turn', () => {
    seedBranchedTurn();
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('branch-selector-strip')).toBeInTheDocument();
    expect(getByTestId('branch-gutter')).toBeInTheDocument();
    // Active is var-1 after the sibling arrives (spec §15.1 — new variant
    // becomes active). Strip position reflects this.
    expect(getByTestId('branch-strip-label').textContent).toBe('variant 2 of 2');
  });

  it('filters non-active variants from the transcript', () => {
    seedBranchedTurn();
    const { queryByText, getByText } = render(() => <ChatPane />);
    expect(getByText('variant one')).toBeInTheDocument();
    // Root variant is hidden because it is not the active one.
    expect(queryByText('root answer')).toBeNull();
  });

  it('next-arrow dispatches select_branch with the sibling variant index', () => {
    seedBranchedTurn();
    const { getByTestId } = render(() => <ChatPane />);
    invokeMock.mockClear();
    fireEvent.click(getByTestId('branch-strip-next'));
    // neighbourVariantId(next) wraps from var-1 back to root-1 (index 0).
    expect(invokeMock).toHaveBeenCalledWith('select_branch', {
      sessionId: SID,
      parentId: 'root-1',
      variantIndex: 0,
    });
  });

  it('info button opens the metadata popover', () => {
    seedBranchedTurn();
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('branch-metadata-popover')).toBeNull();
    fireEvent.click(getByTestId('branch-strip-info'));
    expect(getByTestId('branch-metadata-popover')).toBeInTheDocument();
  });

  it('popover Delete dispatches delete_branch with the variant index', () => {
    seedBranchedTurn();
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('branch-strip-info'));
    invokeMock.mockClear();
    fireEvent.click(getByTestId('branch-popover-delete-1'));
    expect(invokeMock).toHaveBeenCalledWith('delete_branch', {
      sessionId: SID,
      parentId: 'root-1',
      variantIndex: 1,
    });
  });

  it('Export button writes the active branch path (only the selected variant) to the clipboard as JSON', async () => {
    seedBranchedTurn();
    // jsdom doesn't have a clipboard; stub it. Cleaned up after the test so
    // later clipboard-dependent tests don't inherit the stub.
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
    try {
      const { getByTestId } = render(() => <ChatPane />);
      fireEvent.click(getByTestId('branch-strip-info'));
      fireEvent.click(getByTestId('branch-popover-export'));
      // writeText fires asynchronously; yield microtasks.
      await Promise.resolve();
      await Promise.resolve();
      expect(writeText).toHaveBeenCalledTimes(1);
      const jsonArg = writeText.mock.calls[0]![0] as string;
      const parsed = JSON.parse(jsonArg) as Array<Record<string, unknown>>;
      // Must contain the user turn plus the ACTIVE assistant variant only.
      const roles = parsed.map((r) => r.role);
      expect(roles).toContain('user');
      expect(roles).toContain('assistant');
      const assistantTexts = parsed
        .filter((r) => r.role === 'assistant')
        .map((r) => r.text as string);
      // Active is `var-1` after sibling registration — its text must appear.
      expect(assistantTexts).toContain('variant one');
      // Inactive `root-1` variant must NOT leak into the export.
      expect(assistantTexts).not.toContain('root answer');
    } finally {
      // Remove the stub so it doesn't leak into unrelated tests below.
      // `delete navigator.clipboard` is the mirror of defineProperty.
      Reflect.deleteProperty(navigator, 'clipboard');
    }
  });

  it('renders a gutter per branched group when two branch groups stack', () => {
    seedBranchedTurn();
    pushEvent(SID, { kind: 'UserMessage', text: 'follow-up', message_id: 'u2' });
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'follow root',
      message_id: 'root-2',
      branch_parent: null,
      branch_variant_index: 0,
    });
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'follow var',
      message_id: 'root-2-var-1',
      branch_parent: 'root-2',
      branch_variant_index: 1,
    });
    const { getAllByTestId } = render(() => <ChatPane />);
    const gutters = getAllByTestId('branch-gutter');
    expect(gutters).toHaveLength(2);
  });

  it('BranchSelected event flips which variant is rendered', () => {
    seedBranchedTurn();
    const { getByText, queryByText } = render(() => <ChatPane />);
    expect(getByText('variant one')).toBeInTheDocument();
    // Flip back to root.
    pushEvent(SID, {
      kind: 'BranchSelected',
      parent: 'root-1',
      selected: 'root-1',
    });
    expect(getByText('root answer')).toBeInTheDocument();
    expect(queryByText('variant one')).toBeNull();
  });

  // F-600 DoD: "Switching branches in `BranchSelectorStrip` correctly swaps
  // variant-specific child messages." When the user clicks the strip's prev
  // arrow, the IPC fires AND the rendered variant content swaps in the same
  // tick that the BranchSelected event lands. Combined with the gutter being
  // mounted per-group, this is the round-trip the DoD describes.
  it('strip prev-arrow swaps the rendered variant content on BranchSelected ack', () => {
    seedBranchedTurn();
    const { getByTestId, getByText, queryByText } = render(() => <ChatPane />);
    // Start on var-1 (latest sibling).
    expect(getByText('variant one')).toBeInTheDocument();
    invokeMock.mockClear();

    fireEvent.click(getByTestId('branch-strip-prev'));
    // Strip dispatched select_branch back to the root.
    expect(invokeMock).toHaveBeenCalledWith('select_branch', {
      sessionId: SID,
      parentId: 'root-1',
      variantIndex: 0,
    });

    // Simulate the daemon's BranchSelected ack landing in the store.
    pushEvent(SID, {
      kind: 'BranchSelected',
      parent: 'root-1',
      selected: 'root-1',
    });

    // The transcript now renders the root variant; the previously-active
    // sibling is hidden. The gutter (variant-specific child indicator) is
    // still mounted because the group has 2 live variants.
    expect(getByText('root answer')).toBeInTheDocument();
    expect(queryByText('variant one')).toBeNull();
    expect(getByTestId('branch-gutter')).toBeInTheDocument();
  });

  // F-600 DoD: "Delete-variant button in `BranchMetadataPopover` emits
  // `BranchDeleted` and updates the gutter."
  //
  // The popover's delete fires `delete_branch` (asserted in another test);
  // the daemon then emits `BranchDeleted`. When the deletion drops the live
  // count to 1, the strip + gutter unmount per spec §15.2 (single-variant
  // turns render without chrome). This test asserts that round-trip.
  it('BranchDeleted unmounts the gutter when only one live variant remains', () => {
    seedBranchedTurn();
    const { queryByTestId } = render(() => <ChatPane />);
    // Two variants — gutter visible.
    expect(queryByTestId('branch-gutter')).toBeInTheDocument();

    pushEvent(SID, {
      kind: 'BranchDeleted',
      parent: 'root-1',
      variant_index: 1,
    });

    // After deletion the group has one live variant; spec §15.2 → no chrome.
    expect(queryByTestId('branch-gutter')).toBeNull();
    expect(queryByTestId('branch-selector-strip')).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// F-600 — Re-run popover wiring on the assistant bubble
// ---------------------------------------------------------------------------

describe('ChatPane — re-run popover (F-600)', () => {
  it('renders a re-run trigger on a finalised assistant turn', () => {
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'finalised',
      message_id: 'assist-rerun-1',
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('rerun-trigger-assist-rerun-1')).toBeInTheDocument();
  });

  it('does not render the re-run trigger on a streaming turn', () => {
    pushEvent(SID, {
      kind: 'AssistantDelta',
      delta: 'streaming',
      message_id: 'assist-streaming-1',
    });
    const { queryByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('rerun-trigger-assist-streaming-1')).toBeNull();
  });

  it('clicking the re-run trigger opens the popover with all three variants', () => {
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'pick a variant',
      message_id: 'assist-rerun-2',
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('rerun-trigger-assist-rerun-2'));
    expect(getByTestId('rerun-popover')).toBeInTheDocument();
    expect(getByTestId('rerun-popover-replace')).toBeInTheDocument();
    expect(getByTestId('rerun-popover-branch')).toBeInTheDocument();
    expect(getByTestId('rerun-popover-fresh')).toBeInTheDocument();
  });

  it('Fresh button dispatches rerun_message with variant Fresh', () => {
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'about to be re-run',
      message_id: 'assist-rerun-3',
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('rerun-trigger-assist-rerun-3'));
    invokeMock.mockClear();
    fireEvent.click(getByTestId('rerun-popover-fresh'));
    expect(invokeMock).toHaveBeenCalledWith('rerun_message', {
      sessionId: SID,
      msgId: 'assist-rerun-3',
      variant: 'Fresh',
    });
  });

  it('Replace button dispatches rerun_message with variant Replace', () => {
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'about to be re-run',
      message_id: 'assist-rerun-4',
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('rerun-trigger-assist-rerun-4'));
    invokeMock.mockClear();
    fireEvent.click(getByTestId('rerun-popover-replace'));
    expect(invokeMock).toHaveBeenCalledWith('rerun_message', {
      sessionId: SID,
      msgId: 'assist-rerun-4',
      variant: 'Replace',
    });
  });

  it('Branch button dispatches rerun_message with variant Branch', () => {
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'about to be re-run',
      message_id: 'assist-rerun-5',
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('rerun-trigger-assist-rerun-5'));
    invokeMock.mockClear();
    fireEvent.click(getByTestId('rerun-popover-branch'));
    expect(invokeMock).toHaveBeenCalledWith('rerun_message', {
      sessionId: SID,
      msgId: 'assist-rerun-5',
      variant: 'Branch',
    });
  });

  it('picking a variant closes the popover', () => {
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'about to be re-run',
      message_id: 'assist-rerun-6',
    });
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('rerun-trigger-assist-rerun-6'));
    expect(getByTestId('rerun-popover')).toBeInTheDocument();
    fireEvent.click(getByTestId('rerun-popover-fresh'));
    expect(queryByTestId('rerun-popover')).toBeNull();
  });

  it('the re-run trigger is also rendered on a branched assistant turn', () => {
    // Seed a branched turn so the BranchedAssistantTurn wrapper renders.
    pushEvent(SID, { kind: 'UserMessage', text: 'ask', message_id: 'u1' });
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'root answer',
      message_id: 'br-root-1',
      branch_parent: null,
      branch_variant_index: 0,
    });
    pushEvent(SID, {
      kind: 'AssistantMessage',
      text: 'sibling',
      message_id: 'br-var-1',
      branch_parent: 'br-root-1',
      branch_variant_index: 1,
    });
    const { getByTestId } = render(() => <ChatPane />);
    // Active is br-var-1; its bubble must carry the trigger.
    expect(getByTestId('rerun-trigger-br-var-1')).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// F-391: Composer Stop + Send buttons
// ---------------------------------------------------------------------------
//
// chat-pane.md §4.1 bottom bar: `[Stop] [Send ⌘↵]`
// - Stop is visible only while streaming, flips to primary/ember.
// - Esc while streaming cancels (same as Stop click).
// - Send is a real primary/ember button, UPPERCASE label, disabled while
//   streaming, wired to the same handler as bare-Enter.
describe('Composer Stop button (F-391)', () => {
  it('Stop button is not rendered when not streaming', () => {
    setAwaitingResponse(SID, false);
    const { queryByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('composer-stop-btn')).not.toBeInTheDocument();
  });

  it('Stop button is rendered while streaming', () => {
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('composer-stop-btn')).toBeInTheDocument();
  });

  it('Stop button uses primary (ember) class while streaming', () => {
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);
    const stop = getByTestId('composer-stop-btn');
    expect(stop.className).toContain('composer__btn--primary');
  });

  // F-411 (V7): per voice-terminology.md §8, button labels are verb+noun in
  // display caps. The composer's in-stream cancel button must read
  // "STOP TURN" — naked "STOP" is a verb without a noun object and an
  // earlier bare "Stop" regressed casing to mixed-case.
  it('Stop button renders literal "STOP TURN" text for SR/voice parity', () => {
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);
    const stop = getByTestId('composer-stop-btn');
    expect(stop.textContent?.trim()).toBe('STOP TURN');
  });

  it('clicking Stop invokes session_cancel with the active sessionId', () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);

    fireEvent.click(getByTestId('composer-stop-btn'));

    const cancelCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === 'session_cancel',
    );
    expect(cancelCalls).toHaveLength(1);
    expect(cancelCalls[0]?.[1]).toEqual({ sessionId: SID });
  });

  it('clicking Stop locally re-enables the composer (clears awaiting + streaming)', () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    // Simulate a mid-stream state: awaitingResponse has flipped false but
    // a streamingMessageId is still set.
    pushEvent(SID, { kind: 'AssistantDelta', delta: 'partial', message_id: 'streaming-1' });
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;
    expect(textarea).toBeDisabled();

    fireEvent.click(getByTestId('composer-stop-btn'));

    expect(textarea).not.toBeDisabled();
  });

  it('Esc on textarea while streaming fires session_cancel', () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.keyDown(textarea, { key: 'Escape' });

    const cancelCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === 'session_cancel',
    );
    expect(cancelCalls).toHaveLength(1);
    expect(cancelCalls[0]?.[1]).toEqual({ sessionId: SID });
  });

  it('Esc on textarea when not streaming does not fire session_cancel', () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    setAwaitingResponse(SID, false);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.keyDown(textarea, { key: 'Escape' });

    const cancelCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === 'session_cancel',
    );
    expect(cancelCalls).toHaveLength(0);
  });

  it('surfaces an error turn when session_cancel rejects', async () => {
    invokeMock.mockReset();
    invokeMock.mockRejectedValueOnce(new Error('cancel boom'));
    setAwaitingResponse(SID, true);
    const { getByTestId, findByText } = render(() => <ChatPane />);

    fireEvent.click(getByTestId('composer-stop-btn'));

    await flushMicrotasks();
    expect(await findByText(/cancel boom/)).toBeInTheDocument();
  });
});

describe('Composer Send button (F-391)', () => {
  it('renders a Send button at all times', () => {
    setAwaitingResponse(SID, false);
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('composer-send-btn')).toBeInTheDocument();
  });

  it('Send button label is UPPERCASE SEND', () => {
    setAwaitingResponse(SID, false);
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('composer-send-btn').textContent).toContain('SEND');
  });

  it('Send button uses primary (ember) class', () => {
    setAwaitingResponse(SID, false);
    const { getByTestId } = render(() => <ChatPane />);
    const send = getByTestId('composer-send-btn');
    expect(send.className).toContain('composer__btn--primary');
  });

  it('Send button is disabled while streaming', () => {
    setAwaitingResponse(SID, true);
    const { getByTestId } = render(() => <ChatPane />);
    const send = getByTestId('composer-send-btn') as HTMLButtonElement;
    expect(send.disabled).toBe(true);
  });

  it('Send button is enabled when idle', () => {
    setAwaitingResponse(SID, false);
    const { getByTestId } = render(() => <ChatPane />);
    const send = getByTestId('composer-send-btn') as HTMLButtonElement;
    expect(send.disabled).toBe(false);
  });

  it('clicking Send invokes session_send_message with the trimmed text', () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.input(textarea, { target: { value: 'click send' } });
    fireEvent.click(getByTestId('composer-send-btn'));

    expect(invokeMock).toHaveBeenCalledWith('session_send_message', {
      sessionId: SID,
      text: 'click send',
    });
  });

  it('clicking Send with empty text does not fire session_send_message', () => {
    invokeMock.mockReset();
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('composer-send-btn'));
    const sendCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === 'session_send_message',
    );
    expect(sendCalls).toHaveLength(0);
  });

  it('clicking Send clears the textarea', () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    const { getByTestId } = render(() => <ChatPane />);
    const textarea = getByTestId('composer-textarea') as HTMLTextAreaElement;

    fireEvent.input(textarea, { target: { value: 'hi' } });
    fireEvent.click(getByTestId('composer-send-btn'));

    expect(textarea.value).toBe('');
  });

  it('Send button click and Enter key take the same code path', () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);

    // Path A: Enter
    const a = render(() => <ChatPane />);
    const taA = a.getByTestId('composer-textarea') as HTMLTextAreaElement;
    fireEvent.input(taA, { target: { value: 'same path' } });
    fireEvent.keyDown(taA, { key: 'Enter' });
    const viaEnter = invokeMock.mock.calls.filter(
      (c) => c[0] === 'session_send_message',
    );
    a.unmount();

    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    resetMessagesStore();
    setActiveSessionId(SID);

    // Path B: Send click
    const b = render(() => <ChatPane />);
    const taB = b.getByTestId('composer-textarea') as HTMLTextAreaElement;
    fireEvent.input(taB, { target: { value: 'same path' } });
    fireEvent.click(b.getByTestId('composer-send-btn'));
    const viaClick = invokeMock.mock.calls.filter(
      (c) => c[0] === 'session_send_message',
    );

    expect(viaClick).toEqual(viaEnter);
  });
});

// ---------------------------------------------------------------------------
// F-447 — ToolCallCard Phase 3: expanded body + parallel-reads grouping
// ---------------------------------------------------------------------------

describe('ToolCallCard Phase 3 — icon kind (F-447)', () => {
  it('tags pure-read tools with data-tool-kind="read"', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-kind-read',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt' }),
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-card-tc-kind-read')).toHaveAttribute(
      'data-tool-kind',
      'read',
    );
  });

  it('tags agent-spawn tools with data-tool-kind="agent"', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-kind-agent',
      tool_name: 'agent.spawn',
      args_json: '{}',
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-card-tc-kind-agent')).toHaveAttribute(
      'data-tool-kind',
      'agent',
    );
  });

  it('tags everything else with data-tool-kind="general"', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-kind-general',
      tool_name: 'fs.write',
      args_json: JSON.stringify({ path: '/tmp/x' }),
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-card-tc-kind-general')).toHaveAttribute(
      'data-tool-kind',
      'general',
    );
  });
});

describe('ToolCallCard Phase 3 — status glyph (F-447)', () => {
  it('renders ✓ for completed calls', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-glyph-ok',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-glyph-ok',
      result_summary: '{"ok":true}',
      result_ok: true,
      duration_ms: 12,
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-status-tc-glyph-ok')).toHaveTextContent('✓');
  });

  it('renders ✗ for errored calls', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-glyph-err',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallFailed',
      tool_call_id: 'tc-glyph-err',
      error: 'ENOENT',
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-status-tc-glyph-err')).toHaveTextContent('✗');
  });

  it('renders ! for awaiting-approval calls', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-glyph-wait',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-glyph-wait',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
      preview: { description: 'Edit' },
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-status-tc-glyph-wait')).toHaveTextContent('!');
  });
});

describe('ToolCallCard Phase 3 — duration readout (F-447)', () => {
  it('renders the wire-reported duration in the collapsed row', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-dur',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-dur',
      result_summary: '{"ok":true}',
      result_ok: true,
      duration_ms: 42,
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-duration-tc-dur')).toHaveTextContent('42ms');
  });

  it('collapses to "awaiting approval" while approval is pending', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-aa',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/x' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-aa',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/x' }),
      preview: { description: 'Edit' },
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-duration-tc-aa')).toHaveTextContent(
      'awaiting approval',
    );
  });
});

describe('ToolCallCard Phase 3 — expand/collapse (F-447)', () => {
  it('collapses a completed card by default and expands on click', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-xp',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-xp',
      result_summary: '{"ok":true,"preview":"hello"}',
      result_ok: true,
      result_preview: 'hello',
      duration_ms: 10,
    });
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    const card = getByTestId('tool-call-card-tc-xp');
    expect(card).toHaveAttribute('data-expanded', 'false');
    expect(queryByTestId('tool-call-body-tc-xp')).not.toBeInTheDocument();

    fireEvent.click(getByTestId('tool-call-row-tc-xp'));
    expect(card).toHaveAttribute('data-expanded', 'true');
    expect(getByTestId('tool-call-body-tc-xp')).toBeInTheDocument();
  });

  it('toggles expanded on keyboard activation (Enter)', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-kb',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-kb',
      result_summary: '{}',
      result_ok: true,
      duration_ms: 3,
    });
    const { getByTestId } = render(() => <ChatPane />);
    const row = getByTestId('tool-call-row-tc-kb');
    fireEvent.keyDown(row, { key: 'Enter' });
    expect(getByTestId('tool-call-card-tc-kb')).toHaveAttribute(
      'data-expanded',
      'true',
    );
    fireEvent.keyDown(row, { key: 'Enter' });
    expect(getByTestId('tool-call-card-tc-kb')).toHaveAttribute(
      'data-expanded',
      'false',
    );
  });

  // F-447 follow-up (PR #547 review): the row's Enter/Space handler must be a
  // no-op when the card is awaiting approval — otherwise it collapses the diff
  // the user just read AND the keydown bubbles to ApprovalPrompt's native
  // listener, which interprets Enter as "approve once."
  it('ignores Enter on the row while awaiting-approval (does not collapse the open body)', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-aa-kb',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-aa-kb',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
      preview: { description: '--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y' },
    });
    const { getByTestId } = render(() => <ChatPane />);
    const card = getByTestId('tool-call-card-tc-aa-kb');
    // awaiting-approval cards open by default
    expect(card).toHaveAttribute('data-expanded', 'true');

    fireEvent.keyDown(getByTestId('tool-call-row-tc-aa-kb'), { key: 'Enter' });
    // Row handler was a no-op — body is still expanded.
    expect(card).toHaveAttribute('data-expanded', 'true');
  });

  it('ignores Space on the row while awaiting-approval (duplicate-activate guard)', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-aa-sp',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/bar.ts' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-aa-sp',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/bar.ts' }),
      preview: { description: 'Edit' },
    });
    const { getByTestId } = render(() => <ChatPane />);
    const card = getByTestId('tool-call-card-tc-aa-sp');
    expect(card).toHaveAttribute('data-expanded', 'true');

    const row = getByTestId('tool-call-row-tc-aa-sp');
    fireEvent.keyDown(row, { key: ' ' });
    expect(card).toHaveAttribute('data-expanded', 'true');
  });

  // F-688 / Issue #724: keyboard activation target on the awaiting-approval
  // card must agree with ARIA semantics (WCAG 2.4.3). The single activation
  // target is the inner row — it carries `role="button"`, `tabIndex={0}`,
  // and the click handler. The outer card must NOT have a tabIndex (so it
  // can't appear in the tab order without a role) and Enter on the row must
  // bubble to the ApprovalPrompt's container listener and dispatch approval.
  it('places the awaiting-approval activation target on the row with role+tabindex+onClick agreed', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-a11y-1',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-a11y-1',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
      preview: { description: 'Edit /src/foo.ts' },
    });
    const { getByTestId } = render(() => <ChatPane />);
    const card = getByTestId('tool-call-card-tc-a11y-1');
    const row = getByTestId('tool-call-row-tc-a11y-1');

    // The outer card must not be in the tab order — no tabIndex attribute.
    expect(card).not.toHaveAttribute('tabindex');

    // The row is the single activation target: role="button", tabindex=0,
    // and an onClick handler (the click toggles when not awaiting; during
    // awaiting-approval Enter/Space bubble through to approve via the
    // prompt's container-level listener).
    expect(row).toHaveAttribute('role', 'button');
    expect(row).toHaveAttribute('tabindex', '0');
  });

  it('keyboard Enter on the awaiting-approval row dispatches session_approve_tool', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-a11y-enter',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-a11y-enter',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
      preview: { description: 'Edit /src/foo.ts' },
    });
    const { getByTestId } = render(() => <ChatPane />);
    const row = getByTestId('tool-call-row-tc-a11y-enter');
    invokeMock.mockClear();

    // fireEvent.keyDown bubbles by default; the ApprovalPrompt's container
    // listener (attached on the outer card) sees Enter and approves Once.
    fireEvent.keyDown(row, { key: 'Enter', bubbles: true });

    expect(invokeMock).toHaveBeenCalledWith('session_approve_tool', {
      sessionId: SID,
      toolCallId: 'tc-a11y-enter',
      scope: 'Once',
    });
  });
});

describe('ToolCallCard Phase 3 — expanded body (F-447)', () => {
  it('pretty-prints args JSON in the expanded body', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-args',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt', lines: [1, 2, 3] }),
    });
    pushEvent(SID, {
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-args',
      result_summary: '{}',
      result_ok: true,
      duration_ms: 3,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('tool-call-row-tc-args'));
    const argsBlock = getByTestId('tool-call-args-json-tc-args');
    // Pretty-printing produces a multi-line indented body.
    expect(argsBlock.textContent).toContain('\n');
    expect(argsBlock.textContent).toContain('"path": "a.txt"');
  });

  it('renders the result preview and caps it at 800 chars with "show more"', () => {
    const big = 'y'.repeat(2000);
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-big',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-big',
      result_summary: '{}',
      result_ok: true,
      result_preview: big,
      duration_ms: 5,
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('tool-call-row-tc-big'));
    const pre = getByTestId('tool-call-result-tc-big');
    expect((pre.textContent ?? '').length).toBe(800);
    const showMore = getByTestId('tool-call-show-more-tc-big');
    fireEvent.click(showMore);
    const pre2 = getByTestId('tool-call-result-tc-big');
    expect((pre2.textContent ?? '').length).toBe(2000);
  });

  it('renders a diff/command preview block for destructive tools', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-edit',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-edit',
      tool_name: 'fs.edit',
      args_json: JSON.stringify({ path: '/src/foo.ts' }),
      preview: { description: '--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y' },
    });
    const { getByTestId } = render(() => <ChatPane />);
    // awaiting-approval expands by default
    const diff = getByTestId('tool-call-diff-tc-edit');
    expect(diff).toBeInTheDocument();
    expect(diff.textContent).toContain('+++ b');
  });

  it('surfaces the error payload in a dedicated block when errored', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-badfail',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'missing.txt' }),
    });
    pushEvent(SID, {
      kind: 'ToolCallFailed',
      tool_call_id: 'tc-badfail',
      error: 'ENOENT: no such file',
    });
    const { getByTestId } = render(() => <ChatPane />);
    fireEvent.click(getByTestId('tool-call-row-tc-badfail'));
    expect(getByTestId('tool-call-error-tc-badfail')).toHaveTextContent('ENOENT');
  });
});

describe('ToolCallCard Phase 3 — parallel-reads grouping (F-447 §5.1)', () => {
  it('collapses consecutive reads with shared batch_id into one aggregate card', () => {
    for (const id of ['tc-pr-1', 'tc-pr-2', 'tc-pr-3']) {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: id,
        tool_name: 'fs.read',
        args_json: JSON.stringify({ path: `${id}.txt` }),
        batch_id: '7',
      });
    }
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-group-7')).toBeInTheDocument();
    expect(getByTestId('tool-call-group-count-7')).toHaveTextContent('3 calls');
    // Individual cards are NOT rendered at the top level — they're nested
    // inside the group body (hidden while collapsed).
    expect(queryByTestId('tool-call-body-tc-pr-1')).not.toBeInTheDocument();
  });

  it('reports the max duration across children as the aggregate duration', () => {
    for (const id of ['tc-dur-1', 'tc-dur-2']) {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: id,
        tool_name: 'fs.read',
        args_json: JSON.stringify({ path: `${id}.txt` }),
        batch_id: '9',
      });
    }
    pushEvent(SID, {
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-dur-1',
      result_summary: '{}',
      result_ok: true,
      duration_ms: 10,
    });
    pushEvent(SID, {
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-dur-2',
      result_summary: '{}',
      result_ok: true,
      duration_ms: 25,
    });
    const { getByTestId } = render(() => <ChatPane />);
    expect(getByTestId('tool-call-group-duration-9')).toHaveTextContent('25ms');
  });

  it('expands the group to show each child card', () => {
    for (const id of ['tc-xp-1', 'tc-xp-2']) {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: id,
        tool_name: 'fs.read',
        args_json: JSON.stringify({ path: `${id}.txt` }),
        batch_id: '11',
      });
    }
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('tool-call-group-body-11')).not.toBeInTheDocument();
    fireEvent.click(getByTestId('tool-call-group-row-11'));
    expect(getByTestId('tool-call-group-body-11')).toBeInTheDocument();
    expect(getByTestId('tool-call-card-tc-xp-1')).toBeInTheDocument();
    expect(getByTestId('tool-call-card-tc-xp-2')).toBeInTheDocument();
  });

  // F-696: header is `role="button"` with summary text only — assistive tech
  // benefits from an explicit aria-label that names the count and aggregate
  // status so the full context announces alongside the role.
  it('aggregate header carries an aria-label with count + aggregate status', () => {
    for (const id of ['tc-lbl-1', 'tc-lbl-2', 'tc-lbl-3']) {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: id,
        tool_name: 'fs.read',
        args_json: JSON.stringify({ path: `${id}.txt` }),
        batch_id: 'lbl',
      });
    }
    const { getByTestId } = render(() => <ChatPane />);
    const row = getByTestId('tool-call-group-row-lbl');
    const label = row.getAttribute('aria-label');
    expect(label).toBeTruthy();
    expect(label).toMatch(/parallel reads/i);
    expect(label).toMatch(/3 calls/);
    // Aggregate status is in-progress while no children have completed.
    expect(label).toMatch(/in-progress/i);
  });

  it('renders a single call with batch_id as a standalone card (no group chrome)', () => {
    pushEvent(SID, {
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-solo',
      tool_name: 'fs.read',
      args_json: JSON.stringify({ path: 'a.txt' }),
      batch_id: '42',
    });
    const { getByTestId, queryByTestId } = render(() => <ChatPane />);
    expect(queryByTestId('tool-call-group-42')).not.toBeInTheDocument();
    expect(getByTestId('tool-call-card-tc-solo')).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// F-447 — CSS drift fixture
// ---------------------------------------------------------------------------
//
// Visual regression for the Phase 3 surfaces. The gate asserts the CSS
// source pins spec §5's load-bearing values: the tinted expanded-state
// background (`rgba(255,209,102,0.04)` + `0.15` border), the `--r-sm`
// radius, the nested group's `--sp-5` indent with a 2px `--color-border-1`
// left edge, and the `.tool-call-card` class name (Phase 3 supersedes the
// `.tool-placeholder` name per the DoD). Driven off the source file the
// same way `ChatPane.css` drift was pinned in F-086 — jsdom doesn't
// resolve external stylesheets, so the CSS source is the contract.

describe('ToolCallCard Phase 3 — CSS drift fixture (F-447)', () => {
  const cssSource = readFileSync(
    resolve(__dirname, 'ChatPane.css'),
    'utf-8',
  );

  it('declares the .tool-call-card class (replacing .tool-placeholder)', () => {
    expect(cssSource).toMatch(/\.tool-call-card\s*\{/);
    // `.tool-placeholder` must not appear as a live selector — only
    // referenced inside comments (the migration note).
    const liveSelectorMatch = cssSource.match(/^\s*\.tool-placeholder/m);
    expect(liveSelectorMatch).toBeNull();
  });

  it('pins the expanded-state tinted background and border per spec §5', () => {
    const expandedRule = cssSource.match(
      /\.tool-call-card--expanded\s*\{([^}]*)\}/,
    );
    expect(expandedRule).not.toBeNull();
    const body = expandedRule?.[1] ?? '';
    expect(body).toMatch(/rgba\(\s*255\s*,\s*209\s*,\s*102\s*,\s*0\.04\s*\)/);
    expect(body).toMatch(/rgba\(\s*255\s*,\s*209\s*,\s*102\s*,\s*0\.15\s*\)/);
  });

  it('uses --r-sm for the card radius', () => {
    const cardRule = cssSource.match(/\.tool-call-card\s*\{([^}]*)\}/);
    expect(cardRule?.[1] ?? '').toMatch(/border-radius\s*:\s*var\(--r-sm\)/);
  });

  it('indents nested children --sp-5 with a 2px --color-border-1 left edge', () => {
    const bodyRule = cssSource.match(
      /\.tool-call-group__body\s*\{([^}]*)\}/,
    );
    expect(bodyRule).not.toBeNull();
    const body = bodyRule?.[1] ?? '';
    expect(body).toMatch(/var\(--sp-5\)/);
    expect(body).toMatch(/border-left\s*:\s*2px\s+solid\s+var\(--color-border-1\)/);
  });

  it('maps tool-kind attribute to its spec §5 icon color token', () => {
    expect(cssSource).toMatch(
      /\.tool-call-card\[data-tool-kind='read'\]\s+\.tool-call-card__icon\s*\{[^}]*var\(--color-info\)/,
    );
    expect(cssSource).toMatch(
      /\.tool-call-card\[data-tool-kind='agent'\]\s+\.tool-call-card__icon\s*\{[^}]*var\(--color-ember-400\)/,
    );
    // General default — lives on the base .tool-call-card__icon rule.
    const iconRule = cssSource.match(/\.tool-call-card__icon\s*\{([^}]*)\}/);
    expect(iconRule?.[1] ?? '').toMatch(/var\(--color-ember-100\)/);
  });

  it('rotates the chevron 90deg when the card is expanded', () => {
    expect(cssSource).toMatch(
      /\.tool-call-card--expanded\s*>\s*\.tool-call-card__row\s*>\s*\.tool-call-card__chevron\s*\{[^}]*transform\s*:\s*rotate\(90deg\)/,
    );
  });
});

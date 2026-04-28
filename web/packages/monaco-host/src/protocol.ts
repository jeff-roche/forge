// F-121: parent <-> iframe postMessage protocol.
//
// This module is deliberately free of any `monaco-editor` or
// `monaco-languageclient` import so it can be exercised in jsdom without
// pulling Monaco's workers/globals. The iframe boot wires a real Monaco
// instance into `createIframeProtocol`; tests pass a stub editor.

/** Opaque handle for a document the parent has asked us to edit. */
export type DocumentUri = string;

/** Parent -> iframe: editor lifecycle messages. */
export type EditorInboundMessage =
  | {
      kind: 'open';
      /** Stable document URI (e.g. `file:///path/to/foo.rs`). */
      uri: DocumentUri;
      /** Monaco language id (e.g. `rust`, `typescript`). */
      languageId: string;
      /** Initial buffer contents. */
      value: string;
    }
  | { kind: 'close'; uri: DocumentUri }
  | { kind: 'save'; uri: DocumentUri }
  | { kind: 'focus' }
  | {
      /**
       * LSP response/notification flowing from the parent-side relay back
       * into the iframe's language client. F-123 will wire the parent to
       * `forge-lsp`; until then the parent never emits these and the
       * language client stays idle. Inbound `client.message` and
       * `client.notification` are accepted symmetrically — LSP is
       * bidirectional and the parent may forward `window/logMessage` or
       * similar server-initiated notifications.
       */
      kind: 'client.message' | 'client.notification';
      /** Opaque vscode-jsonrpc Message payload. */
      payload: unknown;
    };

/** Iframe -> parent: editor + LSP outbound messages. */
export type EditorOutboundMessage =
  | { kind: 'ready' }
  | { kind: 'opened'; uri: DocumentUri }
  | { kind: 'closed'; uri: DocumentUri }
  | {
      kind: 'save';
      uri: DocumentUri;
      /** Buffer contents at save time. */
      value: string;
    }
  | {
      kind: 'change';
      uri: DocumentUri;
      /** Full post-change buffer contents (simple v1 protocol). */
      value: string;
    }
  | {
      /** LSP request/notification flowing out to the parent relay. */
      kind: 'client.message';
      payload: unknown;
    }
  | {
      /** LSP notification from the client (fire-and-forget). */
      kind: 'client.notification';
      payload: unknown;
    }
  | {
      /**
       * F-687: keystroke forwarded from the iframe so the parent can
       * react to keys (e.g. Escape) that fire while focus is trapped
       * inside the iframe and thus never reach the parent's
       * window-level `keydown` listener. Only the `key` is sent — we
       * deliberately do not forward modifier state or the full
       * KeyboardEvent shape; consumers that need richer keystroke
       * routing should extend this kind explicitly.
       */
      kind: 'keydown';
      key: string;
    };

/** Minimal editor surface we need. Real Monaco satisfies this via an adapter. */
export interface EditorLike {
  setValue(value: string): void;
  getValue(): string;
  focus(): void;
  onDidChangeContent(cb: (value: string) => void): { dispose(): void };
  dispose(): void;
}

/** A single subscriber callback. */
export type MessageListener = (data: unknown) => void;

/**
 * Decide whether a parsed JSON-RPC payload is a notification (no `id`
 * with a `method`) or a request/response. String payloads are treated as
 * opaque messages.
 */
function classifyOutbound(payload: unknown): 'client.message' | 'client.notification' {
  if (
    typeof payload === 'object' &&
    payload !== null &&
    typeof (payload as { method?: unknown }).method === 'string' &&
    (payload as { id?: unknown }).id === undefined
  ) {
    return 'client.notification';
  }
  return 'client.message';
}

/**
 * `IWebSocket` (from vscode-ws-jsonrpc) compatible surface, adapted over
 * postMessage. Exposed as a class rather than an interface so both
 * `vscode-ws-jsonrpc` and the protocol router can hold the same instance.
 *
 * The LSP reader/writer pair drives this socket with `client.message`
 * payloads. Editor lifecycle messages (`open`, `close`, etc.) bypass it.
 */
export class IframeSocket {
  private readonly messageListeners: Array<(data: unknown) => void> = [];
  private readonly errorListeners: Array<(reason: unknown) => void> = [];
  private readonly closeListeners: Array<(code: number, reason: string) => void> = [];
  private disposed = false;

  constructor(private readonly post: (msg: EditorOutboundMessage) => void) {}

  send(content: string): void {
    if (this.disposed) return;
    // vscode-ws-jsonrpc stringifies messages before calling send(); we parse
    // here so the parent relay sees structured JSON, not a string-in-string.
    let payload: unknown;
    try {
      payload = JSON.parse(content);
    } catch {
      // Fall back to opaque string; the parent relay can still forward it.
      payload = content;
    }
    // JSON-RPC 2.0: a request without an `id` is a notification. Splitting
    // the outbound kind lets the parent relay skip reply bookkeeping for
    // notifications — important because `forge-lsp` relays may drop them
    // rather than ferry a reply back.
    this.post({ kind: classifyOutbound(payload), payload });
  }

  onMessage(cb: (data: unknown) => void): void {
    this.messageListeners.push(cb);
  }

  onError(cb: (reason: unknown) => void): void {
    this.errorListeners.push(cb);
  }

  onClose(cb: (code: number, reason: string) => void): void {
    this.closeListeners.push(cb);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const cb of this.closeListeners) cb(1000, 'disposed');
  }

  /** Feed an inbound LSP `client.message` payload to subscribed readers. */
  receive(payload: unknown): void {
    if (this.disposed) return;
    // vscode-ws-jsonrpc's WebSocketMessageReader accepts the parsed object
    // shape; string is also fine for it, but we normalise to string for
    // parity with the ws-server reference implementations.
    const data = typeof payload === 'string' ? payload : JSON.stringify(payload);
    for (const cb of this.messageListeners) cb(data);
  }
}

/** What the iframe boot gives the protocol factory. */
export interface IframeProtocolConfig {
  /** The editor (real Monaco or a stub). */
  editor: EditorLike;
  /**
   * Send a message to the parent window. Defaults to
   * `window.parent.postMessage(msg, '*')` when running inside the browser;
   * tests inject a recording stub.
   */
  post: (msg: EditorOutboundMessage) => void;
  /**
   * Subscribe to messages from the parent. Defaults to
   * `window.addEventListener('message', ...)` in the browser; tests drive
   * this directly.
   */
  subscribe: (listener: MessageListener) => { dispose(): void };
}

/** The handles the iframe boot needs to hand to the LSP client. */
export interface IframeProtocolHandles {
  /** Socket adapter suitable for `WebSocketMessageReader`/`...Writer`. */
  socket: IframeSocket;
  /** URI of the currently-open document, if any. */
  currentUri(): DocumentUri | null;
  /** Tear everything down. Idempotent. */
  dispose(): void;
}

/**
 * Wire an editor up to the parent-window postMessage protocol. Returns the
 * socket the LSP client should use, plus a disposer.
 */
export function createIframeProtocol(config: IframeProtocolConfig): IframeProtocolHandles {
  const { editor, post, subscribe } = config;
  const socket = new IframeSocket(post);

  let currentUri: DocumentUri | null = null;

  const changeSub = editor.onDidChangeContent((value) => {
    if (currentUri !== null) {
      post({ kind: 'change', uri: currentUri, value });
    }
  });

  const messageSub = subscribe((data) => {
    const msg = data as EditorInboundMessage;
    if (!msg || typeof msg !== 'object' || typeof (msg as { kind?: unknown }).kind !== 'string') {
      return;
    }
    switch (msg.kind) {
      case 'open':
        currentUri = msg.uri;
        editor.setValue(msg.value);
        post({ kind: 'opened', uri: msg.uri });
        break;
      case 'close':
        if (currentUri === msg.uri) {
          currentUri = null;
          editor.setValue('');
        }
        post({ kind: 'closed', uri: msg.uri });
        break;
      case 'save':
        if (currentUri === msg.uri) {
          post({ kind: 'save', uri: msg.uri, value: editor.getValue() });
        }
        break;
      case 'focus':
        editor.focus();
        break;
      case 'client.message':
      case 'client.notification':
        socket.receive(msg.payload);
        break;
    }
  });

  // Signal to the parent that we are ready to receive `open` messages.
  post({ kind: 'ready' });

  let disposed = false;
  return {
    socket,
    currentUri: () => currentUri,
    dispose() {
      if (disposed) return;
      disposed = true;
      changeSub.dispose();
      messageSub.dispose();
      socket.dispose();
      editor.dispose();
    },
  };
}

/**
 * Default browser `subscribe` that listens on `window.message`, filtering
 * by `event.origin === expectedParentOrigin`. Messages from any other
 * origin are silently dropped — defense-in-depth for F-358 in case CSP
 * `frame-src` or the asset-protocol origin ever relaxes and an unexpected
 * peer gets a handle to this window.
 */
export function browserSubscribe(
  target: Window = window,
  expectedParentOrigin: string = window.location.origin,
): (listener: MessageListener) => { dispose(): void } {
  return (listener) => {
    const handler = (e: MessageEvent): void => {
      if (e.origin !== expectedParentOrigin) return;
      listener(e.data);
    };
    target.addEventListener('message', handler);
    return {
      dispose: () => target.removeEventListener('message', handler),
    };
  };
}

/**
 * Default browser `post` that targets `window.parent` with an explicit
 * `targetOrigin`. Wildcard (`'*'`) targets are rejected at construction
 * time (F-358) — Monaco messages carry the full file buffer, so wildcard
 * targeting would leak contents to any frame that can get a handle on
 * the parent window.
 */
export function browserPost(
  parent: Window | null = window.parent,
  targetOrigin: string = window.location.origin,
): (msg: EditorOutboundMessage) => void {
  if (targetOrigin === '*') {
    throw new Error(
      'browserPost: wildcard target origin ("*") is not allowed; pass the exact parent origin',
    );
  }
  return (msg) => {
    if (parent !== null) {
      parent.postMessage(msg, targetOrigin);
    }
  };
}

// F-121: iframe entry point.
//
// Mounts Monaco into `#editor`, wires the postMessage protocol, and
// constructs (but does not start) the `MonacoLanguageClient`.

import { mountEditor } from './editor.js';
import { createLanguageClient } from './client.js';
import {
  browserPost,
  browserSubscribe,
  createIframeProtocol,
} from './protocol.js';

const host = document.getElementById('editor');
if (host === null) {
  throw new Error('monaco-host: #editor element not found');
}

const editor = mountEditor(host);

// F-358: the default `browserPost` / `browserSubscribe` target and allow
// only `window.location.origin`. Today the iframe is first-party same-origin
// (loaded via a relative URL from the parent `app` bundle), so the parent's
// origin equals the iframe's origin. If the iframe is ever hosted on a
// foreign origin (e.g. Tauri asset-protocol change, CDN), pass the real
// parent origin to both factories — never fall back to `'*'`.
const post = browserPost();
const handles = createIframeProtocol({
  editor,
  post,
  subscribe: browserSubscribe(),
});

// F-687: forward Escape to the parent so modals embedding this iframe
// (e.g. MemoryEditor) can close on a keyboard-only user's first try
// even while focus is trapped inside Monaco. Currently scoped to
// `Escape`; extend the allowlist when more keys need passthrough.
const FORWARDED_KEYS = new Set(['Escape']);
window.addEventListener('keydown', (event) => {
  if (!FORWARDED_KEYS.has(event.key)) return;
  post({ kind: 'keydown', key: event.key });
});

// Construct the LSP client eagerly so the transport path is verified at
// boot. Starting it is F-123's job; see README "LSP lifecycle".
const client = createLanguageClient(handles.socket);

// Expose for debugging only; keep off `window` in production builds.
if (import.meta.env.DEV) {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  (window as any).__forgeMonacoHost = { handles, client };
}

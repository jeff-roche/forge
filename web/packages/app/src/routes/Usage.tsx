// F-594: Usage route — wraps `<UsagePane>` for the dashboard window.
//
// The route's job is to resolve the active workspace root for the
// per-workspace filter. The component itself accepts `null` (cross-workspace
// fallback at the IPC), but on the dashboard the user-facing default is
// "this workspace", so we read the active root from the existing session
// store. `?ws=<root>` overrides it for direct navigation from a session
// window — same pattern the Catalog route follows.

import { type Component } from 'solid-js';
import { useSearchParams } from '@solidjs/router';
import { UsagePane } from '../components/usage/UsagePane';
import { activeWorkspaceRoot } from '../stores/session';
import './Usage.css';

export const Usage: Component = () => {
  const [searchParams] = useSearchParams<{ ws?: string }>();
  const workspaceRoot = (): string | null => {
    const raw = searchParams.ws;
    if (raw && raw.trim().length > 0) return raw;
    return activeWorkspaceRoot();
  };

  return (
    <main class="usage-route">
      <h1 class="usage-route__title">Usage</h1>
      <UsagePane workspaceRoot={workspaceRoot()} />
    </main>
  );
};

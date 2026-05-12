// F-592: Dashboard-mounted Catalog route.
//
// Hosts the `<CatalogPane>` component and pulls its required `workspaceRoot`
// out of the URL query string (`?ws=<workspace_root>`). The route lives on
// the Dashboard window so the pane's `list_*` calls clear the F-591
// `dashboard` window-label gate; the workspace string is validated against
// the registry by the shell-side handler before any disk I/O.
//
// When `?ws=` is missing we render a friendly "open from a session" notice
// instead of fabricating a path — the F-591 contract rejects unregistered
// workspace roots, so silently passing an empty string would only surface
// as a confusing IPC error.

import { type Component } from 'solid-js';
import { useParams, useSearchParams } from '@solidjs/router';
import { CatalogPane, type CatalogKind } from '../components/catalog/CatalogPane';
import { activeWorkspaceRoot } from '../stores/session';
import './Catalog.css';

const KIND_VALUES: readonly CatalogKind[] = ['skills', 'mcp', 'agents'];

function parseKind(raw: string | undefined): CatalogKind | undefined {
  return KIND_VALUES.find((k) => k === raw);
}

export const Catalog: Component = () => {
  const [searchParams] = useSearchParams<{ ws?: string }>();
  const params = useParams<{ kind?: string }>();
  const initialKind = (): CatalogKind | undefined => parseKind(params.kind);

  // Workspace resolution: explicit `?ws=` wins (matches the old behaviour
  // when the user opens the catalog from a session window), otherwise fall
  // back to the dashboard's active workspace. When neither is set we pass
  // an empty string — the backend treats that as "user-tier only" and
  // returns globally-installed skills/MCP/agents from `~/.agent-skills`,
  // `~/.mcp.json`, `~/.agents`. The catalog always renders; per-kind
  // empty hints handle the truly-no-entries case.
  const workspaceRoot = (): string => {
    const raw = searchParams.ws;
    if (raw && raw.trim().length > 0) return raw;
    return activeWorkspaceRoot() ?? '';
  };

  return (
    <main class="catalog-route">
      <CatalogPane workspaceRoot={workspaceRoot()} initialKind={initialKind()} />
    </main>
  );
};

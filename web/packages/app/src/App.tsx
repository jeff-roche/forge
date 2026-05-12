import type { Component } from 'solid-js';
import { Router, Route } from '@solidjs/router';
import { Dashboard } from './routes/Dashboard';
import { SessionWindow } from './routes/Session/SessionWindow';
import { AgentMonitor } from './routes/AgentMonitor';
import { Catalog } from './routes/Catalog';
import { ProvidersPage } from './routes/Providers/ProvidersPage';
import { Usage } from './routes/Usage';
import { AppShell } from './shell/AppShell';

export const App: Component = () => {
  return (
    <Router root={AppShell}>
      <Route path="/" component={Dashboard} />
      <Route path="/session/:id" component={SessionWindow} />
      {/* F-140: session-scoped when we have an id (sub-agents + bg agents
          live per-session), session-agnostic fallback for the dashboard
          entry point until F-138 wires the status-bar agent badge. */}
      <Route path="/agents" component={AgentMonitor} />
      <Route path="/agents/:id" component={AgentMonitor} />
      {/* F-592: Catalog route. `?ws=<workspace_root>` selects the workspace;
          missing parameter falls back to the active workspace, then to a
          friendly empty state. The optional `:kind` segment lets the
          sidebar nav rows (`/catalog/skills`, `/catalog/mcp`,
          `/catalog/agents`) land on the matching tab pre-selected. */}
      <Route path="/catalog/:kind?" component={Catalog} />
      {/* F-594: Usage view — chart + limits + per-model breakdown over the
          F-593 `usage_summary` IPC. Mounts on the dashboard window. */}
      <Route path="/usage" component={Usage} />
      {/* F-729: Providers page — full-page editor surface for
          configured providers. F-730–F-733 fill the per-row actions. */}
      <Route path="/providers" component={ProvidersPage} />
    </Router>
  );
};

import {
  type Component,
  type JSX,
  Show,
  createSignal,
  onCleanup,
  onMount,
} from 'solid-js';
import { useMatch } from '@solidjs/router';
import { ActivityBar, type ActivityId } from './ActivityBar';
import { FilesSidebar } from './FilesSidebar';
import { Sidebar } from './Sidebar';
import { StatusBar } from './StatusBar';
import { activeOpenFile, activeWorkspaceRoot } from '../stores/session';
import { CommandPalette, registerBuiltins } from '../commands';
import './AppShell.css';

/**
 * F-716: route-agnostic chrome. Mounts the ActivityBar + StatusBar on every
 * top-level route and surfaces the workspace FilesSidebar inside the Session
 * route. Sits at the Router `root` so navigating between routes never
 * unmounts the chrome.
 */
export const AppShell: Component<{ children?: JSX.Element }> = (props) => {
  // F-157 / F-153: register built-in palette entries once the Router context
  // is available — AppShell is the first child of `<Router>`.
  registerBuiltins();

  // The activity bar's selection drives the optional sidebar slot. Only
  // wired on the session route — FilesSidebar is workspace chrome and has
  // no meaning on Dashboard / Usage / Catalog / AgentMonitor.
  const sessionMatch = useMatch(() => '/session/*');
  const isSessionRoute = () => sessionMatch() !== undefined;

  const [activeActivity, setActiveActivity] = createSignal<ActivityId | null>(null);

  const toggleFiles = (): void => {
    setActiveActivity((prev) => (prev === 'files' ? null : 'files'));
  };

  const onActivitySelect = (id: ActivityId): void => {
    // Only 'files' is wired today. Search/Git are visible-but-disabled.
    if (id !== 'files') return;
    toggleFiles();
  };

  // Cmd/Ctrl+Shift+E mirrors the activity-bar Files toggle so the shortcut
  // works regardless of which surface holds keyboard focus.
  const onShortcut = (e: KeyboardEvent): void => {
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.shiftKey && (e.key === 'E' || e.key === 'e')) {
      e.preventDefault();
      toggleFiles();
    }
  };

  onMount(() => {
    window.addEventListener('keydown', onShortcut);
  });
  onCleanup(() => {
    window.removeEventListener('keydown', onShortcut);
  });

  const showFilesSidebar = (): boolean =>
    isSessionRoute() &&
    activeActivity() === 'files' &&
    activeWorkspaceRoot() !== null;

  const onFileOpen = (path: string): void => {
    const handler = activeOpenFile();
    if (handler === null) {
      console.warn('openFile dropped — no active session bridge', path);
      return;
    }
    handler(path);
  };

  return (
    <div class="app-shell" data-testid="app-shell">
      <div class="app-shell__body">
        <ActivityBar
          active={activeActivity()}
          onSelect={onActivitySelect}
        />
        <Sidebar />
        <Show when={showFilesSidebar()}>
          <FilesSidebar
            workspaceRoot={activeWorkspaceRoot() as string}
            onOpen={onFileOpen}
          />
        </Show>
        <main class="app-shell__content">
          {props.children}
        </main>
      </div>
      <StatusBar />
      <CommandPalette />
    </div>
  );
};

// F-126: files sidebar — tree view bound to the `tree` Tauri command.
//
// Per docs/ui-specs/layout-panes.md §3.2, the files sidebar is NOT a pane
// type. It lives in the activity-bar-driven sidebar slot. Toggle lives in
// the parent (SessionWindow) so a keyboard shortcut can show/hide without
// reaching through a ref.
//
// The tree is loaded on mount from the session's workspace root via the
// F-122 `tree` command (which now honors `.gitignore` via F-126's
// gitignored walker in `forge-fs`). Context-menu actions dispatch to:
//   - `open` → `props.onOpen(path)`; SessionWindow is expected to route
//     the path to a new/existing EditorPane.
//   - `rename` → `rename_path` Tauri command; reloads the tree on success.
//   - `delete` → `delete_path` Tauri command; reloads the tree on success.
//
// The tree-load surface is injectable (`loadTree` prop) for tests, matching
// EditorPane's style.

import {
  type Component,
  For,
  Show,
  createEffect,
  createMemo,
  createSignal,
  on,
  onCleanup,
  onMount,
} from 'solid-js';
import { createStore } from 'solid-js/store';
import { IconButton, MenuItem } from '@forge/design';
import type { TreeNodeDto } from '@forge/ipc';
import {
  tree as defaultTree,
  renamePath as defaultRenamePath,
  deletePath as defaultDeletePath,
} from '../ipc/fs';
import { activeSessionId } from '../stores/session';
import './FilesSidebar.css';

export interface FilesSidebarProps {
  /** Absolute workspace root; passed as the `tree(root)` argument. */
  workspaceRoot: string;
  /** Called when the user activates a file (double-click or context-menu
   *  "Open"). SessionWindow is expected to open an EditorPane on the path. */
  onOpen: (path: string) => void;
  /** Injection seam for tests. */
  loadTree?: typeof defaultTree;
  renamePath?: typeof defaultRenamePath;
  deletePath?: typeof defaultDeletePath;
  /** Injection seam for user-facing confirms. Tests inject deterministic
   *  stubs; production resolves to `window.prompt` / `window.confirm`. */
  promptForRename?: (currentName: string) => string | null;
  confirmDelete?: (name: string) => boolean;
}

type ContextMenuTarget = {
  path: string;
  name: string;
  isDir: boolean;
  x: number;
  y: number;
};

export const FilesSidebar: Component<FilesSidebarProps> = (props) => {
  const load = () => props.loadTree ?? defaultTree;
  const rename = () => props.renamePath ?? defaultRenamePath;
  const remove = () => props.deletePath ?? defaultDeletePath;
  const askRename = () =>
    props.promptForRename ?? ((name: string) => window.prompt('Rename to:', name));
  const askDelete = () =>
    props.confirmDelete ?? ((name: string) => window.confirm(`Delete ${name}?`));

  const [rootNode, setRootNode] = createSignal<TreeNodeDto | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [isLoading, setIsLoading] = createSignal(false);
  // F-573: keyed store so each TreeRow's `isExpanded(path)` access subscribes
  // only to its own path key. A toggle invalidates one key, not the whole
  // map — O(1) reactive cost instead of O(N) on 5–10k node trees.
  const [expanded, setExpanded] = createStore<Record<string, boolean>>({});
  const isExpanded = (path: string): boolean => expanded[path] === true;
  const [menu, setMenu] = createSignal<ContextMenuTarget | null>(null);

  const refresh = async (): Promise<void> => {
    const sid = activeSessionId();
    if (sid === null) return;
    setIsLoading(true);
    try {
      const next = await load()(sid, props.workspaceRoot);
      setRootNode(next);
      // Auto-expand the root so the first level of children is visible.
      setExpanded(next.path, true);
      setError(null);
    } catch (err) {
      setError(errorToString(err));
    } finally {
      setIsLoading(false);
    }
  };

  onMount(() => {
    void refresh();
  });

  createEffect(
    on(
      () => [activeSessionId(), props.workspaceRoot] as const,
      () => {
        void refresh();
      },
      { defer: true },
    ),
  );

  const dismissMenu = (): void => {
    setMenu(null);
  };

  const onEscape = (e: KeyboardEvent): void => {
    if (e.key === 'Escape') dismissMenu();
  };

  onMount(() => {
    window.addEventListener('click', dismissMenu);
    window.addEventListener('keydown', onEscape);
  });
  onCleanup(() => {
    window.removeEventListener('click', dismissMenu);
    window.removeEventListener('keydown', onEscape);
  });

  const toggle = (path: string): void => {
    setExpanded(path, (prev) => !prev);
  };

  const handleContextMenu = (
    e: MouseEvent,
    node: TreeNodeDto,
  ): void => {
    e.preventDefault();
    setMenu({
      path: node.path,
      name: node.name,
      isDir: node.kind === 'Dir',
      x: e.clientX,
      y: e.clientY,
    });
  };

  const doOpen = (path: string): void => {
    dismissMenu();
    props.onOpen(path);
  };

  const doRename = async (): Promise<void> => {
    const target = menu();
    dismissMenu();
    if (!target) return;
    const sid = activeSessionId();
    if (sid === null) return;
    const fresh = askRename()(target.name);
    if (fresh === null || fresh === '' || fresh === target.name) return;
    const parent = target.path.slice(0, target.path.lastIndexOf('/'));
    const nextPath = `${parent}/${fresh}`;
    try {
      await rename()(sid, target.path, nextPath);
      await refresh();
    } catch (err) {
      setError(errorToString(err));
    }
  };

  const doDelete = async (): Promise<void> => {
    const target = menu();
    dismissMenu();
    if (!target) return;
    const sid = activeSessionId();
    if (sid === null) return;
    if (!askDelete()(target.name)) return;
    try {
      await remove()(sid, target.path);
      await refresh();
    } catch (err) {
      setError(errorToString(err));
    }
  };

  const rootChildren = createMemo<TreeNodeDto[]>(() => {
    const r = rootNode();
    if (r === null) return [];
    return r.children ?? [];
  });

  /** Non-null when the root's stats signal truncation or walk errors. */
  const statsNotice = createMemo<string | null>(() => {
    const r = rootNode();
    if (r === null) return null;
    const s = r.stats;
    if (!s) return null;
    const parts: string[] = [];
    if (s.truncated && s.omitted_count > 0) {
      parts.push(`${s.omitted_count} file${s.omitted_count === 1 ? '' : 's'} not shown`);
    } else if (s.truncated) {
      parts.push('tree truncated');
    }
    if (s.error_count > 0) {
      parts.push(`${s.error_count} read error${s.error_count === 1 ? '' : 's'}`);
    }
    return parts.length > 0 ? parts.join(' · ') : null;
  });

  return (
    <aside
      class="files-sidebar"
      aria-label="Files sidebar"
      data-testid="files-sidebar"
    >
      <header class="files-sidebar__header">
        <span class="files-sidebar__title">EXPLORER</span>
        <IconButton
          class="files-sidebar__refresh"
          label="Refresh file tree"
          title="Refresh"
          onClick={() => {
            void refresh();
          }}
          icon={
            <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M4 4v5h5" />
              <path d="M20 20v-5h-5" />
              <path d="M5 9a9 9 0 0 1 14-3l1 3M19 15a9 9 0 0 1-14 3l-1-3" />
            </svg>
          }
        />
      </header>
      <Show when={isLoading()}>
        <div class="files-sidebar__loading" role="status" data-testid="files-sidebar-loading">
          LOADING TREE
        </div>
      </Show>
      <Show when={error() !== null}>
        <div class="files-sidebar__error" role="alert" data-testid="files-sidebar-error">
          {error()}
        </div>
      </Show>
      <Show when={statsNotice() !== null}>
        <div class="files-sidebar__stats-notice" role="status" data-testid="files-sidebar-stats-notice">
          {statsNotice()}
        </div>
      </Show>
      <div class="files-sidebar__tree" role="tree">
        <Show when={!isLoading() && rootChildren().length === 0 && error() === null}>
          <div class="files-sidebar__empty" role="status" data-testid="files-sidebar-empty">
            NO FILES
          </div>
        </Show>
        <For each={rootChildren()}>
          {(child) => (
            <TreeRow
              node={child}
              depth={0}
              isExpanded={isExpanded}
              onToggle={toggle}
              onOpen={doOpen}
              onContext={handleContextMenu}
            />
          )}
        </For>
      </div>
      <Show when={menu()}>
        {(target) => (
          <ul
            class="files-sidebar__menu"
            role="menu"
            data-testid="files-sidebar-menu"
            style={{
              '--menu-y': `${target().y}px`,
              '--menu-x': `${target().x}px`,
            }}
            onClick={(e) => e.stopPropagation()}
          >
            <li role="none">
              <MenuItem
                data-testid="files-sidebar-menu-open"
                disabled={target().isDir}
                onClick={() => doOpen(target().path)}
              >
                OPEN
              </MenuItem>
            </li>
            <li role="none">
              <MenuItem
                data-testid="files-sidebar-menu-rename"
                onClick={() => {
                  void doRename();
                }}
              >
                RENAME
              </MenuItem>
            </li>
            <li role="none">
              <MenuItem
                variant="danger"
                data-testid="files-sidebar-menu-delete"
                onClick={() => {
                  void doDelete();
                }}
              >
                DELETE
              </MenuItem>
            </li>
          </ul>
        )}
      </Show>
    </aside>
  );
};

interface TreeRowProps {
  node: TreeNodeDto;
  depth: number;
  /**
   * F-573: per-path expansion accessor. Reading `isExpanded(path)` only
   * subscribes the row to its own key in the expansion store, so toggling
   * one row no longer invalidates every other row's `isOpen()` access.
   */
  isExpanded: (path: string) => boolean;
  onToggle: (path: string) => void;
  onOpen: (path: string) => void;
  onContext: (e: MouseEvent, node: TreeNodeDto) => void;
}

const TreeRow: Component<TreeRowProps> = (props) => {
  const isDir = (): boolean => props.node.kind === 'Dir';
  const isOpen = (): boolean => props.isExpanded(props.node.path);

  const onClick = (): void => {
    if (isDir()) {
      props.onToggle(props.node.path);
    }
  };

  const onDoubleClick = (): void => {
    if (!isDir()) {
      props.onOpen(props.node.path);
    }
  };

  return (
    <>
      <div
        class="files-sidebar__row"
        classList={{ 'files-sidebar__row--dir': isDir() }}
        role="treeitem"
        aria-expanded={isDir() ? isOpen() : undefined}
        data-testid="files-sidebar-row"
        data-path={props.node.path}
        style={{ '--row-depth': String(props.depth) }}
        onClick={onClick}
        onDblClick={onDoubleClick}
        onContextMenu={(e) => props.onContext(e, props.node)}
      >
        <span class="files-sidebar__chevron" aria-hidden="true">
          {isDir() ? (isOpen() ? '\u25be' : '\u25b8') : ''}
        </span>
        <span class="files-sidebar__name">{props.node.name}</span>
      </div>
      <Show when={isDir() && isOpen()}>
        <For each={props.node.children ?? []}>
          {(child) => (
            <TreeRow
              node={child}
              depth={props.depth + 1}
              isExpanded={props.isExpanded}
              onToggle={props.onToggle}
              onOpen={props.onOpen}
              onContext={props.onContext}
            />
          )}
        </For>
      </Show>
    </>
  );
};

function errorToString(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === 'string') return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

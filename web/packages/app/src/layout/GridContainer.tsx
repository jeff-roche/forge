import { type Component, type JSX, Match, Show, Switch } from 'solid-js';
import type { LayoutTree } from '@forge/ipc';
import { SplitPane } from './SplitPane';
import { DropZoneOverlay } from './DropZoneOverlay';
import type { DragState } from './useDragToDock';
import './GridContainer.css';

/**
 * F-150: GridContainer now consumes the serialized `LayoutTree` shape from
 * the IPC package directly. A leaf carries `pane_type`, which drives the
 * caller-supplied `renderLeaf` dispatcher. Previously each leaf embedded a
 * `render` closure, but closures can't be persisted and made the tree
 * non-serializable, forcing an adapter layer that had to be rebuilt for
 * every tree mutation. Moving to the persisted shape lets drag-to-dock,
 * layoutStore, and GridContainer share one data structure end-to-end.
 */
export type LayoutLeaf = LayoutTree & { kind: 'leaf' };

export interface GridContainerProps {
  /** The root of the layout tree. Arbitrarily deep H/V combinations. */
  tree: LayoutTree;
  /**
   * Produce the pane body for a leaf. GridContainer stays pane-agnostic —
   * the dispatch on `pane_type` lives in the caller's renderer so chat /
   * terminal / editor / files / agent-monitor panes all mount through a
   * single code path. Re-invoked whenever the tree identity changes.
   */
  renderLeaf: (leaf: LayoutLeaf) => JSX.Element;
  /**
   * Emitted when a split node's ratio changes (drag or double-click
   * reset). `id` is the split node's id; the parent owns persistence
   * and tree mutation. GridContainer keeps no state of its own.
   */
  onRatioChange: (id: string, next: number) => void;
  /**
   * Active drag-to-dock state, threaded from `useDragToDock`. When set,
   * GridContainer paints a `DropZoneOverlay` on the targeted leaf so the
   * hovered zone gets the §3.6 ember tint. Optional — omit when drag
   * isn't wired up (e.g. unit tests for ratio behavior).
   */
  dragState?: DragState | null;
}

/**
 * Recursive N-ary layout renderer over `SplitPane`. Each split node maps to
 * one `SplitPane`; each leaf renders via `renderLeaf(leaf)`. GridContainer
 * is a pure function of its props — the tree walks parent-side, which keeps
 * persistence (F-120) and drag-to-dock (F-118) trivial to layer on later.
 *
 * F-118 layered the optional `dragState` prop here so the drop-zone overlay
 * co-locates with the leaves. Tree mutation still happens outside — the
 * `useDragToDock` hook calls `applyDockDrop` and hands the result back to
 * the parent via `onTreeChange`. GridContainer stays stateless.
 */
export const GridContainer: Component<GridContainerProps> = (props) => {
  return (
    <GridNode
      node={props.tree}
      renderLeaf={props.renderLeaf}
      onRatioChange={props.onRatioChange}
      dragState={() => props.dragState ?? null}
    />
  );
};

const GridNode: Component<{
  node: LayoutTree;
  renderLeaf: (leaf: LayoutLeaf) => JSX.Element;
  onRatioChange: (id: string, next: number) => void;
  dragState: () => DragState | null;
}> = (props) => {
  return (
    <Switch>
      <Match when={props.node.kind === 'leaf' && (props.node as LayoutLeaf)}>
        {(leaf) => {
          // F-573: only the targeted leaf mounts a DropZoneOverlay subtree.
          // Previously every leaf mounted/unmounted the overlay on
          // drag-start/end (O(N) DOM churn for an N-pane grid). Gating the
          // `<Show>` on `targetId === leaf().id` collapses that to O(1) —
          // and `activeZone` only re-evaluates inside the targeted leaf's
          // reactive scope, so non-target leaves don't react to pointermove
          // at all.
          const isTarget = () => {
            const s = props.dragState();
            return s !== null && s.targetId === leaf().id;
          };
          const activeZone = () => {
            const s = props.dragState();
            return s !== null && s.targetId === leaf().id ? s.zone : null;
          };
          return (
            <div
              class="grid-leaf"
              data-testid={`grid-leaf-${leaf().id}`}
              data-leaf-id={leaf().id}
            >
              {props.renderLeaf(leaf())}
              <Show when={isTarget()}>
                <DropZoneOverlay activeZone={activeZone()} />
              </Show>
            </div>
          );
        }}
      </Match>
      <Match
        when={
          props.node.kind === 'split' &&
          (props.node as LayoutTree & { kind: 'split' })
        }
      >
        {(split) => (
          <SplitPane
            direction={split().direction}
            ratio={split().ratio}
            onRatioChange={(next) => props.onRatioChange(split().id, next)}
          >
            <GridNode
              node={split().a}
              renderLeaf={props.renderLeaf}
              onRatioChange={props.onRatioChange}
              dragState={props.dragState}
            />
            <GridNode
              node={split().b}
              renderLeaf={props.renderLeaf}
              onRatioChange={props.onRatioChange}
              dragState={props.dragState}
            />
          </SplitPane>
        )}
      </Match>
    </Switch>
  );
};

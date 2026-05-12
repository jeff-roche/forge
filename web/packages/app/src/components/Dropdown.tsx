// Lightweight ARIA combobox for the New Session dialog. Native <select>
// pops a chrome-styled menu (GTK on Linux, etc.) that ignores app CSS;
// this component renders its own trigger + listbox so the dropdown
// matches the surrounding form. Options can carry an optional chip
// rendered in a trailing column (used by the agent picker to show the
// scope an agent was loaded from).

import {
  createEffect,
  createSignal,
  createUniqueId,
  For,
  onCleanup,
  onMount,
  Show,
  type Component,
  type JSX,
} from 'solid-js';
import './Dropdown.css';

export interface DropdownChip {
  text: string;
  /** Tone hint — drives the chip's color treatment. */
  tone?: 'workspace' | 'user' | 'neutral';
}

export interface DropdownOption {
  value: string;
  label: string;
  chip?: DropdownChip;
  disabled?: boolean;
}

export interface DropdownProps {
  value: string;
  options: DropdownOption[];
  onChange: (value: string) => void;
  disabled?: boolean;
  /** Mirrors a read-only `<select>` — surfaces `aria-readonly="true"` on
   * the trigger for assistive tech. Combine with `disabled` to also lock
   * pointer interaction. */
  readonly?: boolean;
  placeholder?: string;
  ariaLabel?: string;
  testid?: string;
  /** Class on the trigger. The popup is portaled into the trigger's
   * parent — surrounding spacing is unaffected. */
  class?: string;
}

export const Dropdown: Component<DropdownProps> = (props) => {
  const [open, setOpen] = createSignal(false);
  const [highlighted, setHighlighted] = createSignal(-1);
  const listboxId = createUniqueId();
  let triggerRef: HTMLButtonElement | undefined;
  let panelRef: HTMLDivElement | undefined;

  const selected = (): DropdownOption | undefined =>
    props.options.find((o) => o.value === props.value);

  const enabledIndices = (): number[] =>
    props.options
      .map((o, i) => (o.disabled ? -1 : i))
      .filter((i) => i >= 0);

  const moveHighlight = (delta: number): void => {
    const idxs = enabledIndices();
    if (idxs.length === 0) return;
    const current = highlighted();
    const currentPos = idxs.indexOf(current);
    const nextPos =
      currentPos === -1
        ? delta > 0
          ? 0
          : idxs.length - 1
        : (currentPos + delta + idxs.length) % idxs.length;
    const next = idxs[nextPos];
    if (next !== undefined) setHighlighted(next);
  };

  const commit = (idx: number): void => {
    const opt = props.options[idx];
    if (!opt || opt.disabled) return;
    props.onChange(opt.value);
    setOpen(false);
    triggerRef?.focus();
  };

  const openPanel = (): void => {
    if (props.disabled) return;
    setOpen(true);
    const idxs = enabledIndices();
    const currentIdx = props.options.findIndex((o) => o.value === props.value);
    setHighlighted(
      idxs.includes(currentIdx) ? currentIdx : (idxs[0] ?? -1),
    );
  };

  const onTriggerKeyDown = (e: KeyboardEvent): void => {
    if (props.disabled) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      if (!open()) {
        openPanel();
      } else if (e.key === 'Enter' || e.key === ' ') {
        if (highlighted() >= 0) commit(highlighted());
      } else {
        moveHighlight(e.key === 'ArrowDown' ? 1 : -1);
      }
      return;
    }
    if (e.key === 'Escape' && open()) {
      e.preventDefault();
      setOpen(false);
    }
  };

  // Outside-click close.
  const onDocPointerDown = (e: PointerEvent): void => {
    if (!open()) return;
    const t = e.target as Node | null;
    if (
      t &&
      !triggerRef?.contains(t) &&
      !panelRef?.contains(t)
    ) {
      setOpen(false);
    }
  };

  onMount(() => {
    document.addEventListener('pointerdown', onDocPointerDown, true);
  });
  onCleanup(() => {
    document.removeEventListener('pointerdown', onDocPointerDown, true);
  });

  // Scroll the highlighted option into view when navigating with the keyboard.
  createEffect(() => {
    if (!open()) return;
    const idx = highlighted();
    if (idx < 0) return;
    const el = panelRef?.querySelector<HTMLElement>(
      `[data-option-index="${idx}"]`,
    );
    // JSDOM (vitest) does not implement scrollIntoView.
    el?.scrollIntoView?.({ block: 'nearest' });
  });

  const renderLabel = (): JSX.Element => {
    const cur = selected();
    if (cur) {
      return (
        <span class="forge-dropdown__label">
          <span class="forge-dropdown__label-text">{cur.label}</span>
          <Show when={cur.chip}>
            {(chip) => (
              <span
                class="forge-dropdown__chip"
                data-tone={chip().tone ?? 'neutral'}
              >
                {chip().text}
              </span>
            )}
          </Show>
        </span>
      );
    }
    return (
      <span class="forge-dropdown__label forge-dropdown__label--placeholder">
        {props.placeholder ?? ''}
      </span>
    );
  };

  return (
    <div class={`forge-dropdown${props.class ? ` ${props.class}` : ''}`}>
      <button
        type="button"
        ref={triggerRef}
        class="forge-dropdown__trigger"
        data-testid={props.testid}
        data-value={props.value}
        aria-haspopup="listbox"
        aria-expanded={open()}
        aria-controls={listboxId}
        aria-label={props.ariaLabel}
        aria-readonly={props.readonly ? 'true' : undefined}
        disabled={props.disabled}
        onClick={() => (open() ? setOpen(false) : openPanel())}
        onKeyDown={onTriggerKeyDown}
      >
        {renderLabel()}
        <span class="forge-dropdown__chevron" aria-hidden="true" />
      </button>
      <Show when={open()}>
        <div
          ref={panelRef}
          class="forge-dropdown__panel"
          role="listbox"
          id={listboxId}
          aria-label={props.ariaLabel}
        >
          <For each={props.options}>
            {(opt, i) => (
              <div
                role="option"
                aria-selected={opt.value === props.value}
                aria-disabled={opt.disabled || undefined}
                data-option-index={i()}
                data-option-value={opt.value}
                class="forge-dropdown__option"
                classList={{
                  'forge-dropdown__option--highlighted': highlighted() === i(),
                  'forge-dropdown__option--selected': opt.value === props.value,
                  'forge-dropdown__option--disabled': !!opt.disabled,
                }}
                onPointerEnter={() => !opt.disabled && setHighlighted(i())}
                onClick={() => commit(i())}
              >
                <span class="forge-dropdown__option-label">{opt.label}</span>
                <Show when={opt.chip}>
                  {(chip) => (
                    <span
                      class="forge-dropdown__chip"
                      data-tone={chip().tone ?? 'neutral'}
                    >
                      {chip().text}
                    </span>
                  )}
                </Show>
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
};

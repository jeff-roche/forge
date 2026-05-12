// F-594: stacked horizontal bar chart for the usage view.
//
// We don't pull in a chart library. A single static `<svg viewBox>` is plenty
// for the "stacked tokens by provider" visualisation called for in #612, and
// it sidesteps both bundle bloat and the design-token enforcement headaches
// that come with shipping a third-party renderer.
//
// The SVG uses a fixed 100×60 unitless viewBox so the parent's CSS controls
// the rendered size — nothing here is in pixels, which keeps the F-389
// inline-style gate satisfied. Provider colors come from CSS variables via
// the `data-provider` attribute (see UsagePane.css).

import { For, Show, type Component } from 'solid-js';

export interface UsageChartSegment {
  /** Provider id (anthropic / openai / local / custom / …). Drives the swatch color. */
  provider: string;
  /** Display label — defaults to `provider` when omitted. */
  label?: string;
  /** Sum of `tokens_in + tokens_out` for this provider over the range. */
  tokens: number;
}

export interface UsageChartProps {
  /** One segment per provider. The chart sums them itself. */
  segments: UsageChartSegment[];
}

/** Map an unknown provider id to the `data-provider` value the CSS understands. */
export function chartProviderClass(provider: string): string {
  const normalised = provider.toLowerCase();
  if (normalised === 'anthropic' || normalised === 'openai') return normalised;
  if (normalised === 'local') return 'local';
  return 'custom';
}

/**
 * The single stacked horizontal bar that drives the chart's primary read.
 * Renders one `<rect>` per provider, side-by-side, summing to the full
 * width. Empty segments are dropped so the legend reads cleanly.
 */
export const UsageChart: Component<UsageChartProps> = (props) => {
  const totals = (): { total: number; segments: UsageChartSegment[] } => {
    const filtered = props.segments.filter((s) => s.tokens > 0);
    const total = filtered.reduce((acc, s) => acc + s.tokens, 0);
    return { total, segments: filtered };
  };

  const layout = (): Array<UsageChartSegment & { xPct: number; widthPct: number }> => {
    const { total, segments } = totals();
    if (total === 0) return [];
    let cursor = 0;
    return segments.map((seg) => {
      const widthPct = (seg.tokens / total) * 100;
      const xPct = cursor;
      cursor += widthPct;
      return { ...seg, xPct, widthPct };
    });
  };

  return (
    <div
      class="usage-pane__chart"
      role="img"
      aria-label={`Token usage by provider, ${totals().total} tokens total`}
    >
      <Show
        when={totals().total > 0}
        fallback={<p class="usage-pane__empty">// no data in range</p>}
      >
        <svg
          class="usage-pane__chart-svg"
          viewBox="0 0 100 60"
          preserveAspectRatio="none"
          aria-hidden="true"
        >
          <rect
            class="usage-pane__chart-bg"
            x="0"
            y="0"
            width="100"
            height="48"
          />
          <For each={layout()}>
            {(seg) => (
              <rect
                class="usage-pane__chart-bar"
                data-provider={chartProviderClass(seg.provider)}
                x={String(seg.xPct)}
                y="0"
                width={String(seg.widthPct)}
                height="48"
              >
                <title>
                  {`${seg.label ?? seg.provider}: ${seg.tokens.toLocaleString()} tokens`}
                </title>
              </rect>
            )}
          </For>
          <line
            class="usage-pane__chart-axis"
            x1="0"
            y1="48"
            x2="100"
            y2="48"
          />
          <text class="usage-pane__chart-axis-text" x="0" y="56">
            0
          </text>
          <text
            class="usage-pane__chart-axis-text"
            x="100"
            y="56"
            text-anchor="end"
          >
            {totals().total.toLocaleString()} tokens
          </text>
        </svg>
        <div class="usage-pane__chart-legend" role="list">
          <For each={totals().segments}>
            {(seg) => (
              <span class="usage-pane__chart-legend-item" role="listitem">
                <span
                  class="usage-pane__chart-legend-swatch"
                  data-provider={chartProviderClass(seg.provider)}
                  aria-hidden="true"
                />
                {seg.label ?? seg.provider} · {seg.tokens.toLocaleString()}
              </span>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
};

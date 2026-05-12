import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render } from '@solidjs/testing-library';
import { MemoryRouter, Route } from '@solidjs/router';
import { FirstRunBanner } from './FirstRunBanner';

describe('FirstRunBanner (F-737)', () => {
  afterEach(() => cleanup());

  // The banner needs a router context to render `<A>` cleanly. Mirrors the
  // pattern used by Dashboard.test.tsx so the test environment matches the
  // app shell at runtime.
  function renderBanner(visible: boolean) {
    return render(() => (
      <MemoryRouter>
        <Route path="/" component={() => <FirstRunBanner visible={visible} />} />
      </MemoryRouter>
    ));
  }

  it('renders nothing when visible=false', () => {
    const { queryByTestId } = renderBanner(false);
    expect(queryByTestId('first-run-banner')).toBeNull();
  });

  it('renders the verbatim headline when visible=true', () => {
    const { getByRole } = renderBanner(true);
    const heading = getByRole('heading', { level: 2 });
    expect(heading.textContent).toBe('Welcome to Forge.');
  });

  it('renders the verbatim subtitle when visible=true', () => {
    const { getByTestId } = renderBanner(true);
    const banner = getByTestId('first-run-banner');
    expect(banner.querySelector('.first-run-banner__subtitle')?.textContent).toBe(
      'Add your first provider to start a session.',
    );
  });

  it('renders the CTA button with the verbatim label', () => {
    const { getByRole } = renderBanner(true);
    expect(getByRole('button', { name: '+ Add provider' })).toBeTruthy();
  });

  it('wraps the CTA in a link to /providers?add=1', () => {
    // The query param signals the providers page to pop the Add Provider
    // modal on mount, so the CTA lands the user mid-flow rather than on
    // a static page they have to click through again.
    const { getByRole } = renderBanner(true);
    const link = getByRole('link', { name: '+ Add provider' });
    expect(link.getAttribute('href')).toBe('/providers?add=1');
  });

  it('marks the region with an accessible label', () => {
    const { getByRole } = renderBanner(true);
    const region = getByRole('region', { name: 'Welcome' });
    expect(region).toBeTruthy();
  });
});

import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';

// AboutTab reads the live app version from the Tauri bundle via getVersion()
// (no hardcoded version prop). Mock it so the component test has a value.
vi.mock('@tauri-apps/api/app', () => ({
  getVersion: () => Promise.resolve('0.2.0'),
}));

import AboutTab from './AboutTab.svelte';

describe('AboutTab', () => {
  it('renders the live app version and description', async () => {
    render(AboutTab);
    expect(await screen.findByText(/v0\.2\.0/)).toBeTruthy();
    expect(screen.getByText(/Auto-offloads your GoPro/)).toBeTruthy();
  });
});

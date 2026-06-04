import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import AboutTab from './AboutTab.svelte';

describe('AboutTab', () => {
  it('renders the version and description', () => {
    render(AboutTab, { props: { version: '0.3.0' } });
    expect(screen.getByText(/0\.3\.0/)).toBeTruthy();
    expect(screen.getByText(/Auto-offloads your GoPro/)).toBeTruthy();
  });
});

import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../lib/bindings', () => ({
  openPath: vi.fn().mockResolvedValue(undefined),
}));

import { openPath } from '../../lib/bindings';
import AdvancedTab from './AdvancedTab.svelte';

describe('AdvancedTab', () => {
  beforeEach(() => vi.clearAllMocks());

  it('shows the resolved config path and destination', () => {
    render(AdvancedTab, { props: { configPath: '/Users/me/GPBeam/gpbeam.toml', destRoot: '/Users/me/GPBeam' } });
    expect(screen.getByText('/Users/me/GPBeam/gpbeam.toml')).toBeTruthy();
  });

  it('opens the destination folder', async () => {
    render(AdvancedTab, { props: { configPath: '/c/gpbeam.toml', destRoot: '/Users/me/GPBeam' } });
    await fireEvent.click(screen.getByText('Open destination folder'));
    expect(openPath).toHaveBeenCalledWith('/Users/me/GPBeam');
  });
});

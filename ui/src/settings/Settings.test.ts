import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

// Mock bindings used by Settings and its child tabs (children import the same module).
vi.mock('../lib/bindings', () => ({
  // Settings now gates on isFirstRun(); false routes straight to the tabs.
  isFirstRun: vi.fn().mockResolvedValue(false),
  getConfig: vi.fn(),
  saveConfig: vi.fn(),
  pickFolder: vi.fn(),
  getHistory: vi.fn().mockResolvedValue([]),
  revealPath: vi.fn().mockResolvedValue(undefined),
  openPath: vi.fn().mockResolvedValue(undefined),
  getAutostart: vi.fn().mockResolvedValue(false),
  setAutostart: vi.fn().mockResolvedValue(undefined),
  setNextcloudCredentials: vi.fn().mockResolvedValue(undefined),
  clearNextcloudCredentials: vi.fn().mockResolvedValue(undefined),
}));

import { getConfig, saveConfig, isFirstRun } from '../lib/bindings';

import Settings from './Settings.svelte';

function makeView() {
  return {
    destRoot: '/Users/me/GPBeam',
    filenameTemplate: '{date}_{original}',
    includeProxies: false,
    includeThumbnails: false,
    verify: true,
    spaceHeadroom: 1073741824,
    deleteAfterVerify: false,
    autoEject: false,
    cloud: null,
  };
}

describe('Settings', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // clearAllMocks wipes return values; keep the first-run gate routing to tabs.
    (isFirstRun as any).mockResolvedValue(false);
  });

  it('loads the config on mount and shows the Destination tab first', async () => {
    (getConfig as any).mockResolvedValue(makeView());
    render(Settings);
    expect(await screen.findByLabelText('Destination folder')).toBeTruthy();
    expect(getConfig).toHaveBeenCalledTimes(1);
  });

  it('switches to the Behavior tab when its sidebar item is clicked', async () => {
    (getConfig as any).mockResolvedValue(makeView());
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Behavior' }));
    expect(screen.getByLabelText('Verify each copied file')).toBeTruthy();
  });

  it('saves and shows a success line', async () => {
    (getConfig as any).mockResolvedValue(makeView());
    (saveConfig as any).mockResolvedValue({ status: 'idle', cloud: { configured: false, pending: 0, failed: 0, paused: false } });
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('Saved.');
    expect(saveConfig).toHaveBeenCalledTimes(1);
    const arg = (saveConfig as any).mock.calls[0][0];
    expect(arg.destRoot).toBe('/Users/me/GPBeam');
  });

  it('shows an inline error when save rejects', async () => {
    (getConfig as any).mockResolvedValue(makeView());
    (saveConfig as any).mockRejectedValue('destination folder cannot be empty');
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByText('destination folder cannot be empty')).toBeTruthy();
  });
});

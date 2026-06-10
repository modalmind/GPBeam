import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

// Mock bindings used by Settings and its child tabs (children import the same module).
vi.mock('../lib/bindings', () => ({
  // Settings now gates on isFirstRun(); false routes straight to the tabs.
  isFirstRun: vi.fn().mockResolvedValue(false),
  getConfig: vi.fn(),
  saveConfig: vi.fn(),
  getConfigPath: vi.fn(),
  pickFolder: vi.fn(),
  completeWizard: vi.fn().mockResolvedValue({}),
  getHistory: vi.fn().mockResolvedValue([]),
  revealPath: vi.fn().mockResolvedValue(undefined),
  openPath: vi.fn().mockResolvedValue(undefined),
  getAutostart: vi.fn().mockResolvedValue(false),
  setAutostart: vi.fn().mockResolvedValue(undefined),
  setNextcloudCredentials: vi.fn().mockResolvedValue(undefined),
  clearNextcloudCredentials: vi.fn().mockResolvedValue(undefined),
  migratePlaintextCredentials: vi.fn().mockResolvedValue(undefined),
}));

// Capture the window-focus listener Settings registers for re-hydration.
const { focusHandlers } = vi.hoisted(() => ({ focusHandlers: [] as Array<() => void> }));
vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({
    listen: (event: string, cb: () => void) => {
      if (event === 'tauri://focus') focusHandlers.push(cb);
      return Promise.resolve(() => {});
    },
    hide: () => Promise.resolve(),
  }),
}));

import {
  getConfig,
  saveConfig,
  isFirstRun,
  getConfigPath,
  clearNextcloudCredentials,
} from '../lib/bindings';

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

function makeCloud() {
  return {
    destinationId: 'nc1',
    baseUrl: 'https://cloud.example.com',
    username: 'alice',
    remoteRoot: 'GoPro',
    mirrorMode: 'auto',
    chunkThreshold: 52428800,
    maxConcurrency: 2,
    maxAttempts: 8,
    hasPassword: true,
  };
}

describe('Settings', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    focusHandlers.length = 0;
    // clearAllMocks wipes return values; keep the first-run gate routing to tabs.
    (isFirstRun as any).mockResolvedValue(false);
    (getConfigPath as any).mockResolvedValue('/Users/me/Library/Application Support/GPBeam/gpbeam.toml');
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

  it('shows the resolved config path from get_config_path on the Advanced tab', async () => {
    (getConfig as any).mockResolvedValue(makeView());
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Advanced' }));
    expect(
      await screen.findByText('/Users/me/Library/Application Support/GPBeam/gpbeam.toml')
    ).toBeTruthy();
  });

  it('falls back to the gpbeam.toml placeholder when get_config_path fails', async () => {
    (getConfig as any).mockResolvedValue(makeView());
    (getConfigPath as any).mockRejectedValue('no window');
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Advanced' }));
    expect(await screen.findByText('gpbeam.toml')).toBeTruthy();
  });

  it('re-fetches the config when the window regains focus with no unsaved edits', async () => {
    (getConfig as any).mockResolvedValue(makeView());
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await waitFor(() => expect(focusHandlers.length).toBeGreaterThan(0));

    // Config changed elsewhere (e.g. wizard / another window) while hidden.
    (getConfig as any).mockResolvedValue({ ...makeView(), destRoot: '/Users/me/Elsewhere' });
    focusHandlers[0]();
    await waitFor(() =>
      expect((screen.getByLabelText('Destination folder') as HTMLInputElement).value).toBe(
        '/Users/me/Elsewhere'
      )
    );
    expect(getConfig).toHaveBeenCalledTimes(2);
  });

  it('does NOT clobber unsaved edits when the window regains focus', async () => {
    (getConfig as any).mockResolvedValue(makeView());
    render(Settings);
    const dest = (await screen.findByLabelText('Destination folder')) as HTMLInputElement;
    await waitFor(() => expect(focusHandlers.length).toBeGreaterThan(0));

    await fireEvent.input(dest, { target: { value: '/Users/me/Edited' } });
    focusHandlers[0]();
    await new Promise((r) => setTimeout(r, 0));
    expect(getConfig).toHaveBeenCalledTimes(1); // initial load only
    expect(dest.value).toBe('/Users/me/Edited');
  });

  it('defers keychain clearing until cloud removal is actually saved', async () => {
    (getConfig as any).mockResolvedValue({ ...makeView(), cloud: makeCloud(), plaintextCredentialIds: [] });
    (saveConfig as any).mockResolvedValue({ status: 'idle' });
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Cloud' }));

    // Toggling mirroring off must not touch the keychain...
    await fireEvent.click(screen.getByLabelText('Enable Nextcloud mirroring'));
    expect(clearNextcloudCredentials).not.toHaveBeenCalled();

    // ...only a successful save with cloud === null clears it, afterwards.
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('Saved.');
    expect((saveConfig as any).mock.calls[0][0].cloud).toBeNull();
    expect(clearNextcloudCredentials).toHaveBeenCalledWith('nc1');
    expect((saveConfig as any).mock.invocationCallOrder[0]).toBeLessThan(
      (clearNextcloudCredentials as any).mock.invocationCallOrder[0]
    );
  });

  it('does not clear credentials when the save fails', async () => {
    (getConfig as any).mockResolvedValue({ ...makeView(), cloud: makeCloud(), plaintextCredentialIds: [] });
    (saveConfig as any).mockRejectedValue('disk on fire');
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Cloud' }));
    await fireEvent.click(screen.getByLabelText('Enable Nextcloud mirroring'));
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('disk on fire');
    expect(clearNextcloudCredentials).not.toHaveBeenCalled();
  });

  it('does not clear credentials when cloud is still configured on save', async () => {
    (getConfig as any).mockResolvedValue({ ...makeView(), cloud: makeCloud(), plaintextCredentialIds: [] });
    (saveConfig as any).mockResolvedValue({ status: 'idle' });
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('Saved.');
    expect(clearNextcloudCredentials).not.toHaveBeenCalled();
  });

  it('normalizes blank/invalid cloud numbers to defaults (floored at 1) before save', async () => {
    (getConfig as any).mockResolvedValue({ ...makeView(), cloud: makeCloud(), plaintextCredentialIds: [] });
    (saveConfig as any).mockResolvedValue({ status: 'idle' });
    render(Settings);
    await screen.findByLabelText('Destination folder');
    await fireEvent.click(screen.getByRole('button', { name: 'Cloud' }));

    // Blank number inputs bind null, which u64/usize reject server-side.
    await fireEvent.input(screen.getByLabelText('Chunk threshold'), { target: { value: '' } });
    await fireEvent.input(screen.getByLabelText('Max concurrency'), { target: { value: '0' } });
    await fireEvent.input(screen.getByLabelText('Max attempts'), { target: { value: '' } });

    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('Saved.');
    const sent = (saveConfig as any).mock.calls[0][0];
    expect(sent.cloud.chunkThreshold).toBe(52428800); // default restored
    expect(sent.cloud.maxConcurrency).toBe(1); // floored at 1
    expect(sent.cloud.maxAttempts).toBe(8); // default restored
  });
});

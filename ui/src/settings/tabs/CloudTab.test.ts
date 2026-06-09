import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../lib/bindings', () => ({
  setNextcloudCredentials: vi.fn().mockResolvedValue(undefined),
  clearNextcloudCredentials: vi.fn().mockResolvedValue(undefined),
  migratePlaintextCredentials: vi.fn().mockResolvedValue(undefined),
}));

import {
  setNextcloudCredentials,
  clearNextcloudCredentials,
  migratePlaintextCredentials,
} from '../../lib/bindings';
import CloudTab from './CloudTab.svelte';

function makeView(cloud: any = null) {
  return {
    destRoot: '/Users/me/GPBeam',
    filenameTemplate: '{date}_{original}',
    includeProxies: false,
    includeThumbnails: false,
    verify: true,
    spaceHeadroom: 1073741824,
    deleteAfterVerify: false,
    autoEject: false,
    wiredIngest: true,
    cloud,
    plaintextCredentialIds: [],
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

describe('CloudTab', () => {
  beforeEach(() => vi.clearAllMocks());

  it('shows the enable toggle off and no fields when cloud is null', () => {
    render(CloudTab, { props: { view: makeView(null) } });
    expect((screen.getByLabelText('Enable Nextcloud mirroring') as HTMLInputElement).checked).toBe(false);
    expect(screen.queryByLabelText('Base URL')).toBeNull();
  });

  it('creates a default cloud view when enabled', async () => {
    const view = makeView(null);
    render(CloudTab, { props: { view } });
    await fireEvent.click(screen.getByLabelText('Enable Nextcloud mirroring'));
    expect(view.cloud).not.toBeNull();
    expect(view.cloud.destinationId).toBe('nc1');
    expect(view.cloud.mirrorMode).toBe('off');
    expect(view.cloud.chunkThreshold).toBe(52428800);
    expect(view.cloud.maxConcurrency).toBe(2);
    expect(view.cloud.maxAttempts).toBe(8);
    expect(view.cloud.hasPassword).toBe(false);
  });

  it('clears cloud and keychain when disabled', async () => {
    const view = makeView(makeCloud());
    render(CloudTab, { props: { view } });
    await fireEvent.click(screen.getByLabelText('Enable Nextcloud mirroring'));
    expect(clearNextcloudCredentials).toHaveBeenCalledWith('nc1');
    expect(view.cloud).toBeNull();
  });

  it('shows Saved when a password is already stored', () => {
    render(CloudTab, { props: { view: makeView(makeCloud()) } });
    expect(screen.getByText('Saved')).toBeTruthy();
  });

  it('stores the app password via the keychain command and marks hasPassword', async () => {
    const cloud = makeCloud();
    cloud.hasPassword = false;
    const view = makeView(cloud);
    render(CloudTab, { props: { view } });
    const pw = screen.getByLabelText('App password') as HTMLInputElement;
    await fireEvent.input(pw, { target: { value: 's3cret-token' } });
    await fireEvent.click(screen.getByText('Save password'));
    await Promise.resolve();
    expect(setNextcloudCredentials).toHaveBeenCalledWith('nc1', 's3cret-token');
    expect(view.cloud.hasPassword).toBe(true);
  });

  it('edits the mirror mode select', async () => {
    const view = makeView(makeCloud());
    render(CloudTab, { props: { view } });
    const sel = screen.getByLabelText('Mirror mode') as HTMLSelectElement;
    await fireEvent.change(sel, { target: { value: 'manual' } });
    expect(view.cloud.mirrorMode).toBe('manual');
  });

  it('shows the plaintext warning and migrates on click', async () => {
    const view: any = makeView(makeCloud());
    view.plaintextCredentialIds = ['nc1'];
    render(CloudTab, { props: { view } });
    expect(screen.getByText(/plain text/i)).toBeTruthy();
    await fireEvent.click(screen.getByText('Move to keychain'));
    await Promise.resolve();
    expect(migratePlaintextCredentials).toHaveBeenCalledWith('nc1');
    expect(view.plaintextCredentialIds).not.toContain('nc1');
    expect(view.cloud.hasPassword).toBe(true);
  });

  it('hides the plaintext warning when the id is not flagged', () => {
    const view: any = makeView(makeCloud());
    view.plaintextCredentialIds = [];
    render(CloudTab, { props: { view } });
    expect(screen.queryByText(/plain text/i)).toBeNull();
  });

  it('warns under Base URL for http to a remote host', () => {
    const view: any = makeView(makeCloud());
    view.cloud.baseUrl = 'http://cloud.example.com';
    render(CloudTab, { props: { view } });
    expect(screen.getByText(/unencrypted/i)).toBeTruthy();
  });

  it('no http warning for https', () => {
    const view: any = makeView(makeCloud());
    view.cloud.baseUrl = 'https://cloud.example.com';
    render(CloudTab, { props: { view } });
    expect(screen.queryByText(/unencrypted/i)).toBeNull();
  });
});

import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../lib/bindings', () => ({
  getAutostart: vi.fn().mockResolvedValue(false),
  setAutostart: vi.fn().mockResolvedValue(undefined),
}));

// The destructive-action prompt MUST go through the dialog plugin: wry's
// WKWebView has no native handler for window.confirm(), which always returns
// false on macOS, so confirm() can never enable the feature in the real app.
vi.mock('@tauri-apps/plugin-dialog', () => ({
  ask: vi.fn(),
}));

import { getAutostart, setAutostart } from '../../lib/bindings';
import { ask } from '@tauri-apps/plugin-dialog';
import BehaviorTab from './BehaviorTab.svelte';

function makeView() {
  return {
    destRoot: '/Users/me/GPBeam',
    filenameTemplate: '{date}_{original}',
    includeProxies: false,
    includeThumbnails: false,
    verify: true,
    spaceHeadroom: 2 * 1024 * 1024 * 1024, // 2 GiB
    deleteAfterVerify: false,
    autoEject: false,
    wiredIngest: true,
    cloud: null,
  };
}

describe('BehaviorTab', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    (getAutostart as any).mockResolvedValue(false);
  });

  it('renders headroom as GiB derived from bytes', () => {
    render(BehaviorTab, { props: { view: makeView() } });
    expect((screen.getByLabelText('Low-disk headroom (GiB)') as HTMLInputElement).value).toBe('2');
  });

  it('writes spaceHeadroom bytes back when GiB changes', async () => {
    const view = makeView();
    render(BehaviorTab, { props: { view } });
    const input = screen.getByLabelText('Low-disk headroom (GiB)') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: '5' } });
    expect(view.spaceHeadroom).toBe(5 * 1024 * 1024 * 1024);
  });

  it('toggles verify directly on the view', async () => {
    const view = makeView();
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Verify each copied file') as HTMLInputElement;
    expect(cb.checked).toBe(true);
    await fireEvent.click(cb);
    expect(view.verify).toBe(false);
  });

  it('prompts via the dialog plugin before enabling delete-after-verify and enables on accept', async () => {
    const view = makeView();
    (ask as any).mockResolvedValue(true);
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Delete files from card after verify') as HTMLInputElement;
    await fireEvent.click(cb);
    await waitFor(() => expect(view.deleteAfterVerify).toBe(true));
    expect(ask).toHaveBeenCalledTimes(1);
    expect((ask as any).mock.calls[0][1]).toMatchObject({ kind: 'warning' });
    await waitFor(() => expect(cb.checked).toBe(true));
  });

  it('does not enable delete-after-verify when the dialog is declined', async () => {
    const view = makeView();
    (ask as any).mockResolvedValue(false);
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Delete files from card after verify') as HTMLInputElement;
    await fireEvent.click(cb);
    await waitFor(() => expect(ask).toHaveBeenCalledTimes(1));
    expect(view.deleteAfterVerify).toBe(false);
    await waitFor(() => expect(cb.checked).toBe(false));
  });

  it('treats a failing dialog as declined', async () => {
    const view = makeView();
    (ask as any).mockRejectedValue(new Error('dialog unavailable'));
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Delete files from card after verify') as HTMLInputElement;
    await fireEvent.click(cb);
    await waitFor(() => expect(ask).toHaveBeenCalledTimes(1));
    expect(view.deleteAfterVerify).toBe(false);
    await waitFor(() => expect(cb.checked).toBe(false));
  });

  it('disabling delete-after-verify does not prompt', async () => {
    const view = makeView();
    view.deleteAfterVerify = true;
    (ask as any).mockResolvedValue(true);
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Delete files from card after verify') as HTMLInputElement;
    await fireEvent.click(cb);
    expect(ask).not.toHaveBeenCalled();
    expect(view.deleteAfterVerify).toBe(false);
  });

  it('reflects autostart on mount and calls setAutostart on toggle', async () => {
    (getAutostart as any).mockResolvedValue(true);
    render(BehaviorTab, { props: { view: makeView() } });
    const cb = await screen.findByLabelText('Launch at login');
    expect((cb as HTMLInputElement).checked).toBe(true);
    await fireEvent.click(cb);
    expect(setAutostart).toHaveBeenCalledWith(false);
  });

  it('reverts the autostart toggle and shows an error when setAutostart fails', async () => {
    (getAutostart as any).mockResolvedValue(false);
    (setAutostart as any).mockRejectedValue('launchd says no');
    render(BehaviorTab, { props: { view: makeView() } });
    const cb = (await screen.findByLabelText('Launch at login')) as HTMLInputElement;
    expect(cb.checked).toBe(false);
    await fireEvent.click(cb);
    expect(await screen.findByText(/launchd says no/)).toBeTruthy();
    await waitFor(() => expect(cb.checked).toBe(false));
  });

  it('reflects wiredIngest and toggles it directly on the view', async () => {
    const view = makeView();
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Offload a USB-connected GoPro') as HTMLInputElement;
    expect(cb.checked).toBe(true);
    await fireEvent.click(cb);
    expect(view.wiredIngest).toBe(false);
    await fireEvent.click(cb);
    expect(view.wiredIngest).toBe(true);
  });
});

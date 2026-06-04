import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../lib/bindings', () => ({
  getAutostart: vi.fn().mockResolvedValue(false),
  setAutostart: vi.fn().mockResolvedValue(undefined),
}));

import { getAutostart, setAutostart } from '../../lib/bindings';
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

  it('prompts confirm before enabling delete-after-verify and only enables on accept', async () => {
    const view = makeView();
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Delete files from card after verify') as HTMLInputElement;
    await fireEvent.click(cb);
    expect(confirmSpy).toHaveBeenCalledTimes(1);
    expect(view.deleteAfterVerify).toBe(true);
    confirmSpy.mockRestore();
  });

  it('does not enable delete-after-verify when confirm is declined', async () => {
    const view = makeView();
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false);
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Delete files from card after verify') as HTMLInputElement;
    await fireEvent.click(cb);
    expect(confirmSpy).toHaveBeenCalledTimes(1);
    expect(view.deleteAfterVerify).toBe(false);
    expect(cb.checked).toBe(false);
    confirmSpy.mockRestore();
  });

  it('disabling delete-after-verify does not prompt', async () => {
    const view = makeView();
    view.deleteAfterVerify = true;
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    render(BehaviorTab, { props: { view } });
    const cb = screen.getByLabelText('Delete files from card after verify') as HTMLInputElement;
    await fireEvent.click(cb);
    expect(confirmSpy).not.toHaveBeenCalled();
    expect(view.deleteAfterVerify).toBe(false);
    confirmSpy.mockRestore();
  });

  it('reflects autostart on mount and calls setAutostart on toggle', async () => {
    (getAutostart as any).mockResolvedValue(true);
    render(BehaviorTab, { props: { view: makeView() } });
    const cb = await screen.findByLabelText('Launch at login');
    expect((cb as HTMLInputElement).checked).toBe(true);
    await fireEvent.click(cb);
    expect(setAutostart).toHaveBeenCalledWith(false);
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

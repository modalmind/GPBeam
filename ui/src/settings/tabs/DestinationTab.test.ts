import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../lib/bindings', () => ({
  pickFolder: vi.fn(),
}));

import { pickFolder } from '../../lib/bindings';
import DestinationTab from './DestinationTab.svelte';

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

describe('DestinationTab', () => {
  beforeEach(() => vi.clearAllMocks());

  it('shows the current destination and template', () => {
    render(DestinationTab, { props: { view: makeView() } });
    expect((screen.getByLabelText('Destination folder') as HTMLInputElement).value).toBe('/Users/me/GPBeam');
    expect((screen.getByLabelText('Filename template') as HTMLInputElement).value).toBe('{date}_{original}');
  });

  it('updates destRoot when the picker returns a path', async () => {
    (pickFolder as any).mockResolvedValue('/Volumes/SSD/Footage');
    const view = makeView();
    render(DestinationTab, { props: { view } });
    await fireEvent.click(screen.getByText('Choose…'));
    await Promise.resolve();
    await Promise.resolve();
    expect((screen.getByLabelText('Destination folder') as HTMLInputElement).value).toBe('/Volumes/SSD/Footage');
  });

  it('leaves destRoot unchanged when the picker is cancelled (null)', async () => {
    (pickFolder as any).mockResolvedValue(null);
    render(DestinationTab, { props: { view: makeView() } });
    await fireEvent.click(screen.getByText('Choose…'));
    await Promise.resolve();
    expect((screen.getByLabelText('Destination folder') as HTMLInputElement).value).toBe('/Users/me/GPBeam');
  });

  it('toggles include-proxies', async () => {
    render(DestinationTab, { props: { view: makeView() } });
    const cb = screen.getByLabelText('Include proxy files (.LRV)') as HTMLInputElement;
    expect(cb.checked).toBe(false);
    await fireEvent.click(cb);
    expect(cb.checked).toBe(true);
  });
});

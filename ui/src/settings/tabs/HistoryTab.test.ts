import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../lib/bindings', () => ({
  getHistory: vi.fn(),
  revealPath: vi.fn().mockResolvedValue(undefined),
}));

import { getHistory, revealPath } from '../../lib/bindings';
import HistoryTab from './HistoryTab.svelte';

const ROWS = [
  { name: 'GX010001.MP4', destPath: '/d/2026/GX010001.MP4', size: 1024 * 1024 * 1024, copiedAt: '2026-06-03T10:00:00Z', cloudStatus: 'mirrored' },
  { name: 'GX010002.MP4', destPath: '/d/2026/GX010002.MP4', size: 512, copiedAt: '2026-06-03T10:05:00Z', cloudStatus: null },
];

describe('HistoryTab', () => {
  beforeEach(() => vi.clearAllMocks());

  it('loads history on mount and renders rows', async () => {
    (getHistory as any).mockResolvedValue(ROWS);
    render(HistoryTab);
    expect(await screen.findByText('GX010001.MP4')).toBeTruthy();
    expect(getHistory).toHaveBeenCalledWith(50);
    // Canonical humanBytes (Phase 7 union, R1) emits IEC/binary units.
    expect(screen.getByText('1.0 GiB')).toBeTruthy();
    expect(screen.getByText('512 B')).toBeTruthy();
    expect(screen.getByText('mirrored')).toBeTruthy();
  });

  it('shows an empty message when there is no history', async () => {
    (getHistory as any).mockResolvedValue([]);
    render(HistoryTab);
    expect(await screen.findByText('No transfers yet.')).toBeTruthy();
  });

  it('reveals a file by its destination path', async () => {
    (getHistory as any).mockResolvedValue(ROWS);
    render(HistoryTab);
    const buttons = await screen.findAllByText('Reveal');
    await fireEvent.click(buttons[0]);
    expect(revealPath).toHaveBeenCalledWith('/d/2026/GX010001.MP4');
  });
});

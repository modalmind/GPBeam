import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/svelte";

const isFirstRun = vi.fn();
const getConfig = vi.fn();
vi.mock("../lib/bindings", () => ({
  isFirstRun: (...a: unknown[]) => isFirstRun(...a),
  getConfig: (...a: unknown[]) => getConfig(...a),
  // Settings + its child tabs import these at module load; stub every one so
  // the component tree resolves under jsdom (M3 reconciliation rule R2).
  saveConfig: vi.fn().mockResolvedValue({}),
  getHistory: vi.fn().mockResolvedValue([]),
  revealPath: vi.fn().mockResolvedValue(undefined),
  openPath: vi.fn().mockResolvedValue(undefined),
  getAutostart: vi.fn().mockResolvedValue(false),
  setAutostart: vi.fn().mockResolvedValue(undefined),
  clearNextcloudCredentials: vi.fn().mockResolvedValue(undefined),
  // Wizard's own imports (resolved through the same mock module):
  pickFolder: vi.fn(),
  completeWizard: vi.fn().mockResolvedValue({}),
  setNextcloudCredentials: vi.fn(),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ hide: vi.fn() }),
}));

// store.ts is pulled in by Settings; stub hydrate/subscribe to no-ops.
vi.mock("../lib/store", () => ({
  appState: { subscribe: (run: (v: unknown) => void) => { run({ status: "idle", cloud: { configured: false, pending: 0, failed: 0, paused: false } }); return () => {}; } },
  hydrate: vi.fn().mockResolvedValue(undefined),
  subscribeState: vi.fn().mockReturnValue(() => {}),
}));

import Settings from "./Settings.svelte";

describe("Settings.svelte first-run gate", () => {
  beforeEach(() => {
    isFirstRun.mockReset();
    getConfig.mockReset();
    getConfig.mockResolvedValue({
      destRoot: "/Users/me/GPBeam",
      filenameTemplate: "{date}_{original}",
      includeProxies: false,
      includeThumbnails: false,
      verify: true,
      spaceHeadroom: 1073741824,
      deleteAfterVerify: false,
      autoEject: false,
      cloud: null,
    });
  });

  it("renders the wizard when isFirstRun() is true", async () => {
    isFirstRun.mockResolvedValue(true);
    render(Settings);
    await waitFor(() => expect(screen.getByText(/Welcome to GPBeam/i)).toBeTruthy());
  });

  it("renders the settings tabs (not the wizard) when isFirstRun() is false", async () => {
    isFirstRun.mockResolvedValue(false);
    render(Settings);
    await waitFor(() => expect(isFirstRun).toHaveBeenCalled());
    expect(screen.queryByText(/Welcome to GPBeam/i)).toBeNull();
  });
});

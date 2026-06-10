import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/svelte";

const isFirstRun = vi.fn();
const getConfig = vi.fn();
const pickFolder = vi.fn();
const completeWizard = vi.fn();
vi.mock("../lib/bindings", () => ({
  isFirstRun: (...a: unknown[]) => isFirstRun(...a),
  getConfig: (...a: unknown[]) => getConfig(...a),
  // Settings + its child tabs import these at module load; stub every one so
  // the component tree resolves under jsdom (M3 reconciliation rule R2).
  saveConfig: vi.fn().mockResolvedValue({}),
  getConfigPath: vi.fn().mockResolvedValue("/Users/me/gpbeam.toml"),
  getHistory: vi.fn().mockResolvedValue([]),
  revealPath: vi.fn().mockResolvedValue(undefined),
  openPath: vi.fn().mockResolvedValue(undefined),
  getAutostart: vi.fn().mockResolvedValue(false),
  setAutostart: vi.fn().mockResolvedValue(undefined),
  clearNextcloudCredentials: vi.fn().mockResolvedValue(undefined),
  migratePlaintextCredentials: vi.fn().mockResolvedValue(undefined),
  // Wizard's own imports (resolved through the same mock module):
  pickFolder: (...a: unknown[]) => pickFolder(...a),
  completeWizard: (...a: unknown[]) => completeWizard(...a),
  setNextcloudCredentials: vi.fn(),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    hide: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockResolvedValue(() => {}),
  }),
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
    pickFolder.mockReset();
    completeWizard.mockReset();
    completeWizard.mockResolvedValue({});
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

  it("re-fetches the config after the wizard completes so tabs show the wizard's values", async () => {
    isFirstRun.mockResolvedValue(true);
    pickFolder.mockResolvedValue("/Volumes/Footage/GPBeam");
    // First fetch happens on mount (pre-wizard defaults); the post-wizard fetch
    // must return the config the wizard just saved, not the stale snapshot.
    getConfig.mockReset();
    getConfig
      .mockResolvedValueOnce({
        destRoot: "/Users/me/GPBeam",
        filenameTemplate: "{date}_{original}",
        includeProxies: false,
        includeThumbnails: false,
        verify: true,
        spaceHeadroom: 1073741824,
        deleteAfterVerify: false,
        autoEject: false,
        cloud: null,
      })
      .mockResolvedValue({
        destRoot: "/Volumes/Footage/GPBeam",
        filenameTemplate: "{date}_{original}",
        includeProxies: false,
        includeThumbnails: false,
        verify: true,
        spaceHeadroom: 1073741824,
        deleteAfterVerify: false,
        autoEject: false,
        cloud: null,
      });

    render(Settings);
    await waitFor(() => expect(screen.getByText(/Welcome to GPBeam/i)).toBeTruthy());

    // Drive the wizard: welcome -> folder -> cloud -> Skip.
    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));
    await fireEvent.click(screen.getByRole("button", { name: /Next/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Skip/i }));

    await waitFor(() => expect(completeWizard).toHaveBeenCalledTimes(1));
    // The tabs must render the refetched (post-wizard) config, not the stale one.
    await waitFor(() => expect(getConfig.mock.calls.length).toBeGreaterThanOrEqual(2));
    const dest = (await screen.findByLabelText("Destination folder")) as HTMLInputElement;
    expect(dest.value).toBe("/Volumes/Footage/GPBeam");
  });
});

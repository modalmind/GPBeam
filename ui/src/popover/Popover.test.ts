import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, fireEvent, cleanup } from "@testing-library/svelte";
import { get } from "svelte/store";
import type { AppState } from "../lib/bindings";

// vi.mock factories are hoisted above top-level consts, so the shared mock
// state must be created via vi.hoisted() to exist when the factories run.
// vi.hoisted runs before ES import bindings initialize, so we cannot use the
// top-level `writable` import here; instead build a tiny Svelte-store-contract
// shim inline (subscribe + set), which is all the component (`$appState`) and
// the tests (`appStateStore.set`) need.
const { invokeMock, listenMock, appStateStore, hydrateMock, subscribeStateMock } =
  vi.hoisted(() => {
    function fakeWritable(initial: AppState) {
      let value = initial;
      const subs = new Set<(v: AppState) => void>();
      return {
        subscribe(run: (v: AppState) => void) {
          subs.add(run);
          run(value);
          return () => subs.delete(run);
        },
        set(next: AppState) {
          value = next;
          for (const run of subs) run(value);
        },
      };
    }
    return {
      invokeMock: vi.fn(async () => undefined),
      listenMock: vi.fn(async () => () => {}),
      // The store snapshot each test drives through the component.
      appStateStore: fakeWritable({
        status: "idle",
        run: null,
        lastRun: null,
        cloud: { configured: false, pending: 0, failed: 0, paused: false, uploading: null },
        message: null,
      }),
      hydrateMock: vi.fn(async () => {}),
      subscribeStateMock: vi.fn(() => () => {}),
    };
  });

// --- Mock the Tauri API surface so hydrate()/subscribeState()/commands are inert. ---
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));

// --- Mock the store so each test drives a known snapshot through the component. ---
vi.mock("../lib/store", () => ({
  appState: appStateStore,
  hydrate: hydrateMock,
  subscribeState: subscribeStateMock,
}));

// Import AFTER the mocks are registered so the component binds to the mocked store/bindings.
import Popover from "./Popover.svelte";

const WORKING: AppState = {
  status: "working",
  run: {
    model: "HERO12",
    serial: "C3401",
    filesDone: 2,
    filesTotal: 5,
    bytesDone: 50 * 1024 * 1024,
    bytesTotal: 100 * 1024 * 1024,
    currentFile: "GX010099.MP4",
    // started 100s ago, 50 MiB done -> rate 0.5 MiB/s -> ~100s ETA -> "1:40"
    startedAtUnix: Math.floor(Date.now() / 1000) - 100,
  },
  lastRun: null,
  cloud: { configured: true, pending: 3, failed: 1, paused: false, uploading: null },
  message: null,
};

const IDLE: AppState = {
  status: "idle",
  run: null,
  lastRun: { copied: 8, skipped: 2, failed: 0, bytes: 3 * 1024 * 1024 * 1024 },
  cloud: { configured: false, pending: 0, failed: 0, paused: false, uploading: null },
  message: null,
};

beforeEach(() => {
  cleanup();
  invokeMock.mockClear();
  hydrateMock.mockClear();
  subscribeStateMock.mockClear();
});

describe("Popover (working state)", () => {
  it("wires hydrate + subscribeState on mount", () => {
    appStateStore.set(WORKING);
    render(Popover);
    expect(hydrateMock).toHaveBeenCalledOnce();
    expect(subscribeStateMock).toHaveBeenCalledOnce();
  });

  it("attaches the state listener BEFORE hydrating (event snapshots must win the race)", () => {
    appStateStore.set(WORKING);
    render(Popover);
    expect(subscribeStateMock.mock.invocationCallOrder[0]).toBeLessThan(
      hydrateMock.mock.invocationCallOrder[0],
    );
  });

  it("names both progress bars for assistive tech", () => {
    appStateStore.set({
      ...WORKING,
      cloud: {
        ...WORKING.cloud,
        uploading: { file: "GX010099.MP4", uploaded: 25, total: 100 },
      },
    });
    const { getByRole } = render(Popover);
    expect(getByRole("progressbar", { name: "Offload progress" })).toBeTruthy();
    expect(getByRole("progressbar", { name: "Upload progress" })).toBeTruthy();
  });

  it("shows the run card with file count, bytes, ETA and device chip", () => {
    // Freeze the clock: startedAtUnix is computed here and nowUnix at component
    // init — a real-time seconds tick in between makes elapsed 101s and flakes
    // the ETA to "1:41". Fake timers pin Date.now so elapsed is exactly 100s.
    vi.useFakeTimers();
    try {
      appStateStore.set({
        ...WORKING,
        run: { ...WORKING.run!, startedAtUnix: Math.floor(Date.now() / 1000) - 100 },
      });
      const { getByTestId } = render(Popover);

      expect(getByTestId("status-word").textContent).toContain("working");
      expect(getByTestId("current-file").textContent).toContain("GX010099.MP4");
      // filesDone=2 -> "file 3 of 5"
      expect(getByTestId("file-count").textContent).toContain("file 3 of 5");
      expect(getByTestId("bytes").textContent).toContain("50.0 MiB / 100.0 MiB");
      // 50 MiB of 100 MiB at 0.5 MiB/s over 100s elapsed -> remaining 50 MiB -> 100s -> 1:40
      expect(getByTestId("eta").textContent).toContain("1:40");
      expect(getByTestId("device-chip").textContent).toContain("HERO12");
      expect(getByTestId("device-chip").textContent).toContain("C3401");
    } finally {
      vi.useRealTimers();
    }
  });

  it("renders the cloud card counts and toggles pause via the binding", async () => {
    appStateStore.set(WORKING);
    const { getByTestId } = render(Popover);
    expect(getByTestId("cloud-counts").textContent).toContain("3 pending / 1 failed");

    await fireEvent.click(getByTestId("pause-btn"));
    // pauseCloud() -> invoke("pause_cloud") (no-arg command per the bindings contract)
    expect(invokeMock).toHaveBeenCalledWith("pause_cloud");
  });

  it("retries failed uploads via the binding", async () => {
    appStateStore.set(WORKING);
    const { getByTestId } = render(Popover);
    await fireEvent.click(getByTestId("retry-btn"));
    // retryFailedCloud() -> invoke("retry_failed_cloud") (no-arg command)
    expect(invokeMock).toHaveBeenCalledWith("retry_failed_cloud");
  });
});

describe("Popover (idle state)", () => {
  it("shows the last-run summary and no run card", () => {
    appStateStore.set(IDLE);
    const { getByTestId, queryByTestId } = render(Popover);

    expect(getByTestId("status-word").textContent).toContain("idle");
    expect(queryByTestId("run-card")).toBeNull();
    const summary = getByTestId("last-run");
    expect(summary.textContent).toContain("Copied 8, skipped 2, failed 0");
    expect(summary.textContent).toContain("3.0 GiB");
    // cloud not configured -> no cloud card
    expect(queryByTestId("cloud-card")).toBeNull();
  });

  it("calls openPath when Open destination is clicked", async () => {
    appStateStore.set(IDLE);
    const { getByTestId } = render(Popover);
    await fireEvent.click(getByTestId("open-dest"));
    expect(invokeMock).toHaveBeenCalledWith("open_path", expect.anything());
  });
});

// Keep the store reference live so unused-import lint does not trip.
void get;

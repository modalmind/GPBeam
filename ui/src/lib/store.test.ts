import { describe, it, expect, vi, beforeEach } from "vitest";
import { get } from "svelte/store";
import type { AppState } from "./bindings";

const { getStateMock, onStateMock } = vi.hoisted(() => ({
  getStateMock: vi.fn(),
  onStateMock: vi.fn(),
}));

vi.mock("./bindings", () => ({
  getState: getStateMock,
  onState: onStateMock,
}));

import { appState, hydrate, subscribeState } from "./store";

function snap(status: AppState["status"], message: string | null = null): AppState {
  return {
    status,
    run: null,
    lastRun: null,
    cloud: { configured: false, pending: 0, failed: 0, paused: false, uploading: null },
    message,
  };
}

describe("store", () => {
  beforeEach(() => {
    getStateMock.mockReset();
    onStateMock.mockReset();
    // reset to a known default between tests
    appState.set(snap("idle"));
  });

  it("starts at a valid default snapshot (idle, empty cloud)", () => {
    const s = get(appState);
    expect(s.status).toBe("idle");
    expect(s.cloud.pending).toBe(0);
    expect(s.run).toBeNull();
  });

  it("hydrate() pulls getState() into the store", async () => {
    getStateMock.mockResolvedValue(snap("working", "scanning"));
    await hydrate();
    const s = get(appState);
    expect(getStateMock).toHaveBeenCalledOnce();
    expect(s.status).toBe("working");
    expect(s.message).toBe("scanning");
  });

  it("subscribeState() wires onState -> store.set and returns the unlisten", async () => {
    let handler: ((s: AppState) => void) | undefined;
    const unlisten = vi.fn();
    onStateMock.mockImplementation((cb: (s: AppState) => void) => {
      handler = cb;
      return Promise.resolve(unlisten);
    });

    const stop = subscribeState();
    // onState resolves on the microtask queue; flush it.
    await Promise.resolve();
    handler?.(snap("error", "boom"));
    const s = get(appState);
    expect(s.status).toBe("error");
    expect(s.message).toBe("boom");

    await stop();
    expect(unlisten).toHaveBeenCalled();
  });
});

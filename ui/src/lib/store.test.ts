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

  it("hydrate() drops its result when an event snapshot landed during the await", async () => {
    // get_state resolves only when the test says so — deterministically AFTER the event.
    let resolveGet!: (s: AppState) => void;
    getStateMock.mockImplementation(
      () => new Promise<AppState>((res) => (resolveGet = res)),
    );
    let handler: ((s: AppState) => void) | undefined;
    const unlisten = vi.fn();
    onStateMock.mockImplementation((cb: (s: AppState) => void) => {
      handler = cb;
      return Promise.resolve(unlisten);
    });

    const stop = subscribeState();
    const pending = hydrate(); // get_state now in flight
    await Promise.resolve(); // flush onState's microtask so the handler is wired

    handler?.(snap("working", "fresh event")); // fresher event arrives first…
    resolveGet(snap("idle", "stale get_state")); // …then the older invoke resolves
    await pending;

    const s = get(appState);
    expect(s.status).toBe("working");
    expect(s.message).toBe("fresh event");
    await stop();
  });

  it("hydrate() seeds the store again after the listener is detached", async () => {
    const unlisten = vi.fn();
    let handler: ((s: AppState) => void) | undefined;
    onStateMock.mockImplementation((cb: (s: AppState) => void) => {
      handler = cb;
      return Promise.resolve(unlisten);
    });

    const stop = subscribeState();
    await Promise.resolve();
    handler?.(snap("working", "event")); // marks an event as seen
    await stop(); // detached -> guard must reset

    getStateMock.mockResolvedValue(snap("error", "post-detach hydrate"));
    await hydrate();
    expect(get(appState).message).toBe("post-detach hydrate");
  });
});

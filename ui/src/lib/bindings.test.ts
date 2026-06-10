import { describe, it, expect, vi, beforeEach } from "vitest";

// Mock the Tauri core/event modules BEFORE importing the module under test,
// so bindings.ts binds to the mocks. invoke and listen are the only Tauri
// surfaces bindings.ts touches. The mocks are declared via vi.hoisted so they
// are initialized before the hoisted vi.mock factories run.
const { invokeMock, listenMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));

import * as bindings from "./bindings";

describe("bindings", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    listenMock.mockReset();
  });

  it("getState invokes the get_state command and returns its result", async () => {
    const fake = { status: "idle", cloud: { configured: false, pending: 0, failed: 0, paused: false } };
    invokeMock.mockResolvedValue(fake);
    const result = await bindings.getState();
    expect(invokeMock).toHaveBeenCalledWith("get_state");
    expect(result).toBe(fake);
  });

  it("saveConfig forwards the view under a `view` arg", async () => {
    invokeMock.mockResolvedValue({ status: "idle" });
    const view = { destRoot: "/x" } as unknown as bindings.ConfigView;
    await bindings.saveConfig(view);
    expect(invokeMock).toHaveBeenCalledWith("save_config", { view });
  });

  it("setNextcloudCredentials passes destinationId + appPassword", async () => {
    invokeMock.mockResolvedValue(undefined);
    await bindings.setNextcloudCredentials("nc", "secret");
    expect(invokeMock).toHaveBeenCalledWith("set_nextcloud_credentials", {
      destinationId: "nc",
      appPassword: "secret",
    });
  });

  it("getHistory passes the limit arg", async () => {
    invokeMock.mockResolvedValue([]);
    await bindings.getHistory(25);
    expect(invokeMock).toHaveBeenCalledWith("get_history", { limit: 25 });
  });

  it("getConfigPath invokes get_config_path and returns the resolved path", async () => {
    invokeMock.mockResolvedValue("/Users/me/Library/Application Support/GPBeam/gpbeam.toml");
    const path = await bindings.getConfigPath();
    expect(invokeMock).toHaveBeenCalledWith("get_config_path");
    expect(path).toBe("/Users/me/Library/Application Support/GPBeam/gpbeam.toml");
  });

  it("onState subscribes to the gpbeam://state channel and forwards payloads", async () => {
    const unlisten = vi.fn();
    let handler: ((e: { payload: unknown }) => void) | undefined;
    listenMock.mockImplementation((_ch: string, cb: (e: { payload: unknown }) => void) => {
      handler = cb;
      return Promise.resolve(unlisten);
    });
    const cb = vi.fn();
    const stop = await bindings.onState(cb);
    expect(listenMock).toHaveBeenCalledWith("gpbeam://state", expect.any(Function));
    const snap = { status: "working" };
    handler?.({ payload: snap });
    expect(cb).toHaveBeenCalledWith(snap);
    stop();
    expect(unlisten).toHaveBeenCalled();
  });
});

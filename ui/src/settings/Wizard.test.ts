import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, fireEvent, screen, waitFor } from "@testing-library/svelte";

// Mock the Tauri command bindings used by the wizard.
const pickFolder = vi.fn();
const completeWizard = vi.fn();
const setNextcloudCredentials = vi.fn();
vi.mock("../lib/bindings", () => ({
  pickFolder: (...a: unknown[]) => pickFolder(...a),
  completeWizard: (...a: unknown[]) => completeWizard(...a),
  setNextcloudCredentials: (...a: unknown[]) => setNextcloudCredentials(...a),
}));

// Mock the window-hide call.
const hide = vi.fn();
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ hide }),
}));

import Wizard from "./Wizard.svelte";

describe("Wizard.svelte", () => {
  beforeEach(() => {
    pickFolder.mockReset();
    completeWizard.mockReset();
    setNextcloudCredentials.mockReset();
    hide.mockReset();
    completeWizard.mockResolvedValue({ status: "idle", cloud: { configured: false, pending: 0, failed: 0, paused: false } });
  });

  it("renders the Welcome step first", () => {
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" } });
    expect(screen.getByText(/Welcome to GPBeam/i)).toBeTruthy();
    // Folder controls are not shown yet.
    expect(screen.queryByText(/Choose folder/i)).toBeNull();
  });

  it("advancing to the folder step enables Finish only once a folder is chosen", async () => {
    pickFolder.mockResolvedValue("/Volumes/Footage/GPBeam");
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" } });

    // Welcome -> folder step.
    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));

    // Default dest is pre-filled but Finish stays disabled until a folder is *chosen*.
    const next = screen.getByRole("button", { name: /Next/i }) as HTMLButtonElement;
    expect(next.disabled).toBe(true);

    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));

    await waitFor(() => expect((screen.getByRole("button", { name: /Next/i }) as HTMLButtonElement).disabled).toBe(false));
    expect(screen.getByDisplayValue("/Volumes/Footage/GPBeam")).toBeTruthy();
  });

  it("a cancelled folder pick (null) leaves Finish disabled", async () => {
    pickFolder.mockResolvedValue(null);
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" } });
    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));
    expect((screen.getByRole("button", { name: /Next/i }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("finishing (skipping cloud) calls completeWizard with the assembled view then hides the window", async () => {
    pickFolder.mockResolvedValue("/Volumes/Footage/GPBeam");
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" } });

    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));

    // folder step -> cloud step
    await fireEvent.click(screen.getByRole("button", { name: /Next/i }));
    // skip cloud -> finish
    await fireEvent.click(screen.getByRole("button", { name: /Skip/i }));

    await waitFor(() => expect(completeWizard).toHaveBeenCalledTimes(1));
    const view = completeWizard.mock.calls[0][0];
    expect(view.destRoot).toBe("/Volumes/Footage/GPBeam");
    expect(view.cloud).toBeNull();
    // No password entered -> credentials not stored.
    expect(setNextcloudCredentials).not.toHaveBeenCalled();
    await waitFor(() => expect(hide).toHaveBeenCalledTimes(1));
  });

  it("finishing WITH Nextcloud stores credentials then completeWizard carries the cloud view", async () => {
    pickFolder.mockResolvedValue("/Volumes/Footage/GPBeam");
    setNextcloudCredentials.mockResolvedValue(undefined);
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" } });

    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));
    await fireEvent.click(screen.getByRole("button", { name: /Next/i }));

    // cloud step: fill fields then Finish (not Skip).
    await fireEvent.input(screen.getByLabelText(/Base URL/i), { target: { value: "https://cloud.example.com" } });
    await fireEvent.input(screen.getByLabelText(/Username/i), { target: { value: "alice" } });
    await fireEvent.input(screen.getByLabelText(/App password/i), { target: { value: "secret-token" } });
    await fireEvent.input(screen.getByLabelText(/Remote folder/i), { target: { value: "/GoPro" } });

    await fireEvent.click(screen.getByRole("button", { name: /^Finish$/i }));

    await waitFor(() => expect(setNextcloudCredentials).toHaveBeenCalledTimes(1));
    // Credentials are keyed under the shared default destination id (nc1), the
    // same id CloudTab uses, matching the Rust-side defaults.
    expect(setNextcloudCredentials).toHaveBeenCalledWith("nc1", "secret-token");

    await waitFor(() => expect(completeWizard).toHaveBeenCalledTimes(1));
    // The config must be validated/saved BEFORE the secret is stored, so a
    // rejected config never leaves an orphaned keychain entry.
    expect(completeWizard.mock.invocationCallOrder[0]).toBeLessThan(
      setNextcloudCredentials.mock.invocationCallOrder[0],
    );
    const view = completeWizard.mock.calls[0][0];
    expect(view.destRoot).toBe("/Volumes/Footage/GPBeam");
    expect(view.cloud).not.toBeNull();
    expect(view.cloud.destinationId).toBe("nc1");
    expect(view.cloud.baseUrl).toBe("https://cloud.example.com");
    expect(view.cloud.username).toBe("alice");
    expect(view.cloud.hasPassword).toBe(true);
    await waitFor(() => expect(hide).toHaveBeenCalledTimes(1));
  });

  it("does not store credentials when completeWizard rejects (no orphaned secret)", async () => {
    pickFolder.mockResolvedValue("/Volumes/Footage/GPBeam");
    completeWizard.mockRejectedValue("invalid base URL");
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" } });

    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));
    await fireEvent.click(screen.getByRole("button", { name: /Next/i }));

    await fireEvent.input(screen.getByLabelText(/Base URL/i), { target: { value: "https://cloud.example.com" } });
    await fireEvent.input(screen.getByLabelText(/App password/i), { target: { value: "secret-token" } });
    await fireEvent.click(screen.getByRole("button", { name: /^Finish$/i }));

    await waitFor(() => expect(completeWizard).toHaveBeenCalledTimes(1));
    expect(setNextcloudCredentials).not.toHaveBeenCalled();
    expect(hide).not.toHaveBeenCalled();
    expect(await screen.findByText(/invalid base URL/i)).toBeTruthy();
  });

  it("hides the window BEFORE dispatching done, and surfaces a hide failure", async () => {
    pickFolder.mockResolvedValue("/Volumes/Footage/GPBeam");
    hide.mockRejectedValue(new Error("window.hide not allowed"));
    const done = vi.fn();
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" }, events: { done } });

    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));
    await fireEvent.click(screen.getByRole("button", { name: /Next/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Skip/i }));

    await waitFor(() => expect(hide).toHaveBeenCalledTimes(1));
    // The rejection must land on a still-mounted component and be visible.
    expect(await screen.findByText(/window.hide not allowed/i)).toBeTruthy();
    expect(done).not.toHaveBeenCalled();
  });

  it("dispatches done after a successful hide", async () => {
    pickFolder.mockResolvedValue("/Volumes/Footage/GPBeam");
    hide.mockResolvedValue(undefined);
    const done = vi.fn();
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" }, events: { done } });

    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));
    await fireEvent.click(screen.getByRole("button", { name: /Next/i }));
    await fireEvent.click(screen.getByRole("button", { name: /Skip/i }));

    await waitFor(() => expect(done).toHaveBeenCalledTimes(1));
    expect(hide).toHaveBeenCalledTimes(1);
  });

  it("warns inline on a plain-http non-loopback Base URL (and not for https)", async () => {
    render(Wizard, { props: { defaultDest: "/Users/me/GPBeam" } });
    await fireEvent.click(screen.getByRole("button", { name: /Get started/i }));
    pickFolder.mockResolvedValue("/Volumes/Footage/GPBeam");
    await fireEvent.click(screen.getByRole("button", { name: /Choose folder/i }));
    await waitFor(() => expect(pickFolder).toHaveBeenCalledTimes(1));
    await fireEvent.click(screen.getByRole("button", { name: /Next/i }));

    const url = screen.getByLabelText(/Base URL/i);
    await fireEvent.input(url, { target: { value: "http://cloud.example.com" } });
    expect(screen.getByText(/unencrypted/i)).toBeTruthy();

    await fireEvent.input(url, { target: { value: "https://cloud.example.com" } });
    expect(screen.queryByText(/unencrypted/i)).toBeNull();
  });
});

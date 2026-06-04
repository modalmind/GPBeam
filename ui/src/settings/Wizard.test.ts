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
    expect(setNextcloudCredentials).toHaveBeenCalledWith("nextcloud", "secret-token");

    await waitFor(() => expect(completeWizard).toHaveBeenCalledTimes(1));
    const view = completeWizard.mock.calls[0][0];
    expect(view.destRoot).toBe("/Volumes/Footage/GPBeam");
    expect(view.cloud).not.toBeNull();
    expect(view.cloud.baseUrl).toBe("https://cloud.example.com");
    expect(view.cloud.username).toBe("alice");
    expect(view.cloud.hasPassword).toBe(true);
    await waitFor(() => expect(hide).toHaveBeenCalledTimes(1));
  });
});

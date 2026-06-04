// Single Svelte store holding the whole AppState snapshot. The Rust side is the
// source of truth: hydrate() pulls the current snapshot on window show, and
// subscribeState() replaces the store on every gpbeam://state event. No
// business logic lives here — formatting helpers (bytes/eta/percent) go in
// format.ts; reducers live in Rust.
import { writable } from "svelte/store";
import { getState, onState, type AppState } from "./bindings";

function defaultState(): AppState {
  return {
    status: "idle",
    run: null,
    lastRun: null,
    cloud: { configured: false, pending: 0, failed: 0, paused: false, uploading: null },
    message: null,
  };
}

/** The live application snapshot, mirrored from Rust. */
export const appState = writable<AppState>(defaultState());

/** Pull the current snapshot from the backend and seed the store. Safe to call
 *  on every window show; the backend always returns a valid (default) state. */
export async function hydrate(): Promise<void> {
  const s = await getState();
  appState.set(s);
}

/** Wire the gpbeam://state channel into the store. Returns a stop() that
 *  detaches the listener (await it to be sure the unlisten handle resolved). */
export function subscribeState(): () => Promise<void> {
  const pending = onState((s) => appState.set(s));
  return async () => {
    const unlisten = await pending;
    unlisten();
  };
}

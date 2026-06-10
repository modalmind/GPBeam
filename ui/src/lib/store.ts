// Single Svelte store holding the whole AppState snapshot. The Rust side is the
// source of truth: hydrate() seeds the store once on popover mount (the window
// is hidden, not destroyed, so it runs once per app lifetime), and
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

// True once a gpbeam://state event has delivered a snapshot. hydrate() drops
// its get_state result in that case: the event is fresher, and writing the
// slower invoke's result over it would roll the UI back in time.
let eventSnapshotSeen = false;

/** Pull the current snapshot from the backend and seed the store. Runs once on
 *  popover mount; if a gpbeam://state event lands while get_state is still in
 *  flight, the (staler) result is dropped instead of overwriting the event. */
export async function hydrate(): Promise<void> {
  const s = await getState();
  if (eventSnapshotSeen) return;
  appState.set(s);
}

/** Wire the gpbeam://state channel into the store. Returns a stop() that
 *  detaches the listener (await it to be sure the unlisten handle resolved). */
export function subscribeState(): () => Promise<void> {
  const pending = onState((s) => {
    eventSnapshotSeen = true;
    appState.set(s);
  });
  return async () => {
    const unlisten = await pending;
    unlisten();
    // No listener anymore -> a future subscribe+hydrate cycle starts fresh.
    eventSnapshotSeen = false;
  };
}

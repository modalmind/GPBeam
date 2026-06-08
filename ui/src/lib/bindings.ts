// Typed bridge to the Rust command surface (M3 contract).
// - TS types mirror the serde output of src-tauri (camelCase fields).
// - One thin async wrapper per #[tauri::command].
// - onState() subscribes to the whole-AppState snapshot channel.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ---- Mirrors of the serde-serialized Rust types -------------------------

export type Status = "idle" | "working" | "error";

export interface RunProgress {
  model: string | null;
  serial: string | null;
  filesDone: number;
  filesTotal: number;
  bytesDone: number;
  bytesTotal: number;
  currentFile: string | null;
  startedAtUnix: number;
}

export interface UploadProgress {
  file: string;
  uploaded: number;
  total: number;
}

export interface CloudState {
  configured: boolean;
  pending: number;
  failed: number;
  paused: boolean;
  uploading: UploadProgress | null;
}

export interface RunSummaryView {
  copied: number;
  skipped: number;
  failed: number;
  bytes: number;
}

export interface AppState {
  status: Status;
  run: RunProgress | null;
  lastRun: RunSummaryView | null;
  cloud: CloudState;
  message: string | null;
}

export interface CloudView {
  destinationId: string;
  baseUrl: string;
  username: string;
  remoteRoot: string;
  mirrorMode: "off" | "auto" | "manual";
  chunkThreshold: number;
  maxConcurrency: number;
  maxAttempts: number;
  hasPassword: boolean;
}

export interface ConfigView {
  destRoot: string;
  filenameTemplate: string;
  includeProxies: boolean;
  includeThumbnails: boolean;
  verify: boolean;
  spaceHeadroom: number;
  deleteAfterVerify: boolean;
  autoEject: boolean;
  wiredIngest: boolean;
  cloud: CloudView | null;
}

export interface HistoryRow {
  name: string;
  destPath: string;
  size: number;
  copiedAt: string;
  cloudStatus: string | null;
}

// ---- Command wrappers ---------------------------------------------------

export function getState(): Promise<AppState> {
  return invoke<AppState>("get_state");
}

export function getConfig(): Promise<ConfigView> {
  return invoke<ConfigView>("get_config");
}

export function saveConfig(view: ConfigView): Promise<AppState> {
  return invoke<AppState>("save_config", { view });
}

export function pickFolder(): Promise<string | null> {
  return invoke<string | null>("pick_folder");
}

export function openPath(path: string): Promise<void> {
  return invoke<void>("open_path", { path });
}

export function revealPath(path: string): Promise<void> {
  return invoke<void>("reveal_path", { path });
}

/** Show the dedicated settings window (used by the popover's "Settings…"). */
export function openSettings(): Promise<void> {
  return invoke<void>("open_settings");
}

export function setNextcloudCredentials(
  destinationId: string,
  appPassword: string,
): Promise<void> {
  return invoke<void>("set_nextcloud_credentials", { destinationId, appPassword });
}

export function clearNextcloudCredentials(destinationId: string): Promise<void> {
  return invoke<void>("clear_nextcloud_credentials", { destinationId });
}

export function pauseCloud(): Promise<AppState> {
  return invoke<AppState>("pause_cloud");
}

export function resumeCloud(): Promise<AppState> {
  return invoke<AppState>("resume_cloud");
}

export function retryFailedCloud(): Promise<number> {
  return invoke<number>("retry_failed_cloud");
}

export function getHistory(limit: number): Promise<HistoryRow[]> {
  return invoke<HistoryRow[]>("get_history", { limit });
}

export function getAutostart(): Promise<boolean> {
  return invoke<boolean>("get_autostart");
}

export function setAutostart(enabled: boolean): Promise<void> {
  return invoke<void>("set_autostart", { enabled });
}

export function isFirstRun(): Promise<boolean> {
  return invoke<boolean>("is_first_run");
}

export function completeWizard(view: ConfigView): Promise<AppState> {
  return invoke<AppState>("complete_wizard", { view });
}

export function quit(): Promise<void> {
  return invoke<void>("quit");
}

// ---- Whole-AppState snapshot channel ------------------------------------

/** Subscribe to gpbeam://state snapshots. Returns a synchronous stop() that
 *  detaches the listener once Tauri resolves the unlisten handle. */
export async function onState(cb: (s: AppState) => void): Promise<() => void> {
  const unlisten: UnlistenFn = await listen<AppState>("gpbeam://state", (e) => {
    cb(e.payload);
  });
  return unlisten;
}

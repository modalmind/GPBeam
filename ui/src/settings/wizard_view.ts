import type { ConfigView, CloudView } from "../lib/bindings";

/// 1 GiB, matching the Rust `space_headroom` M1 default in `Config::new`.
const DEFAULT_SPACE_HEADROOM = 1073741824;

/**
 * A ConfigView mirroring the Rust M1 defaults (`Config::new(dest)` then
 * `config_to_view`), parameterized by the destination the user picks in the wizard.
 * Returned fresh each call so callers can mutate it freely.
 */
export function defaultConfigView(destRoot: string): ConfigView {
  return {
    destRoot,
    // Must match the Rust Config::new default. Valid tokens: {date} {time} {original}
    // {ext} {camera} {model} — note {original}, NOT {name}, and no "/" (flat layout).
    filenameTemplate: "{date}_{original}",
    includeProxies: false,
    includeThumbnails: false,
    verify: true,
    spaceHeadroom: DEFAULT_SPACE_HEADROOM,
    deleteAfterVerify: false,
    autoEject: false,
    wiredIngest: true,
    cloud: null,
  };
}

/**
 * Return a copy of `view` with `cloud` set to `cloud` (or left null). Never
 * mutates the input view.
 */
export function withCloud(view: ConfigView, cloud: CloudView | null): ConfigView {
  return { ...view, cloud };
}

/** Raw fields collected by the wizard's Nextcloud step. */
export interface CloudFields {
  baseUrl: string;
  username: string;
  appPassword: string;
  remoteRoot: string;
  mirrorMode: "off" | "auto" | "manual";
}

const DEFAULT_CHUNK_THRESHOLD = 10485760; // 10 MiB
const DEFAULT_MAX_CONCURRENCY = 2;
const DEFAULT_MAX_ATTEMPTS = 5;

/**
 * Turn the wizard's Nextcloud fields into a CloudView, or `null` when the user
 * left the base URL blank (i.e. skipped cloud). `hasPassword` reflects whether a
 * non-blank app-password was entered; the password itself is stored separately
 * via `setNextcloudCredentials` and never placed on the view.
 */
export function buildCloudView(fields: CloudFields): CloudView | null {
  const baseUrl = fields.baseUrl.trim();
  if (baseUrl === "") return null;
  return {
    destinationId: "nextcloud",
    baseUrl,
    username: fields.username.trim(),
    remoteRoot: fields.remoteRoot.trim(),
    mirrorMode: fields.mirrorMode,
    chunkThreshold: DEFAULT_CHUNK_THRESHOLD,
    maxConcurrency: DEFAULT_MAX_CONCURRENCY,
    maxAttempts: DEFAULT_MAX_ATTEMPTS,
    hasPassword: fields.appPassword.trim() !== "",
  };
}

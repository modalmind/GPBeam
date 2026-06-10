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

/**
 * The single shared initial CloudView, mirroring the Rust serde defaults
 * (`default_chunk_threshold` 50 MiB, `default_max_concurrency` 2,
 * `default_max_attempts` 8) and the canonical `nc1`/`GoPro` ids used across the
 * backend. Both the wizard and the Cloud tab MUST build from this so keychain
 * entries (keyed by destination_id) land under the same id regardless of which
 * path enabled mirroring. Returned fresh each call so callers can mutate freely.
 */
export function defaultCloudView(): CloudView {
  return {
    destinationId: "nc1",
    baseUrl: "",
    username: "",
    remoteRoot: "GoPro",
    mirrorMode: "off",
    chunkThreshold: 52428800, // 50 MiB
    maxConcurrency: 2,
    maxAttempts: 8,
    hasPassword: false,
  };
}

/**
 * Turn the wizard's Nextcloud fields into a CloudView, or `null` when the user
 * left the base URL blank (i.e. skipped cloud). `hasPassword` reflects whether a
 * non-blank app-password was entered; the password itself is stored separately
 * via `setNextcloudCredentials` and never placed on the view. Everything not
 * collected by the wizard comes from `defaultCloudView()`.
 */
export function buildCloudView(fields: CloudFields): CloudView | null {
  const baseUrl = fields.baseUrl.trim();
  if (baseUrl === "") return null;
  return {
    ...defaultCloudView(),
    baseUrl,
    username: fields.username.trim(),
    remoteRoot: fields.remoteRoot.trim(),
    mirrorMode: fields.mirrorMode,
    hasPassword: fields.appPassword.trim() !== "",
  };
}

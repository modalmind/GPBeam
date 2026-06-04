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
    filenameTemplate: "{date}/{name}",
    includeProxies: false,
    includeThumbnails: false,
    verify: true,
    spaceHeadroom: DEFAULT_SPACE_HEADROOM,
    deleteAfterVerify: false,
    autoEject: false,
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

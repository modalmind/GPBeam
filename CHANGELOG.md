# Changelog

All notable changes to GPBeam are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/).

## [Unreleased]

### Fixed

- Cloud: chunked-upload resume now actually resumes — the deterministic upload
  directory is created on the first attempt and re-found on retry instead of
  restarting from byte 0 under a fresh id each time.
- Cloud: the remote already-present check now compares file size, so a stale
  same-named remote file can no longer mark a job done (and unlock
  delete-after-verify) without uploading.
- Cloud: the https/loopback-only rule is enforced where credentials are used
  (uploader construction), not just on GUI save — hand-edited or pre-0.2 configs
  with a cleartext `http://` server are refused at runtime.
- Cloud: keychain-only setups authenticate correctly — the `[cloud]` username is
  used when the credential store has no username of its own (previously every
  upload failed with an empty username); usernames are percent-encoded in WebDAV
  URLs; credential types redact the password from debug output.
- Cloud: a cross-process worker lock (`.gpbeam-worker.lock` beside the ledger)
  prevents the desktop app and `gpbeam-cli mirror` from draining the same queue
  concurrently (double uploads, clobbered job states); retry backoff keeps its
  anti-thundering-herd jitter and schedules from the failure time, not the pass
  start; the "uploading" event fires when an upload actually starts.
- Wired: HTTP timeouts on camera requests and an idle timeout on downloads — a
  stalled camera no longer wedges the offload, the detector, and SD-card ingest
  behind it.
- Wired: a fully-downloaded `.part` no longer triggers an unsatisfiable range
  request (HTTP 416) that failed the file on every connect; 416 responses are
  treated as stale and restart cleanly.
- Wired: downloads are fsynced before the atomic rename, ledger commit, and any
  camera-side delete (durability now matches the SD path).
- Wired: a camera unplugged and re-plugged during an offload re-fires detection
  instead of sitting idle; one file's rename failure no longer aborts the whole
  run; media-list entries with unparseable sizes are skipped instead of
  re-downloading forever; resumed downloads validate `Content-Range`; heavy
  hashing/rename/ledger work no longer blocks the async runtime.
- Core: crash-recovery "adoption" of an already-present destination file now
  re-verifies content (BLAKE3) instead of trusting byte length, and routes
  through the normal commit path so adopted files are cloud-mirrored — a
  recovered file can no longer silently skip verification or the upload queue.
- Core: ledger record + cloud-job enqueue are atomic; schema migrations run in
  transactions (a mid-migration failure no longer bricks the database) and the
  v2→v3 upgrade is covered by a real migration test.
- Core: a failed delete-after-verify card cleanup is reported as a non-fatal
  warning instead of marking the safely-copied file as failed; files with
  non-UTF-8 names surface as skipped instead of vanishing; `{camera}`/`{model}`
  filename-template tokens are sanitized so a crafted card cannot produce paths
  outside the destination root.
- CLI: exits non-zero when any file or upload fails (was: always 0); unknown
  flags and a value-less `--config` are usage errors instead of being silently
  treated as positional arguments (a typo'd flag could previously leave a stray
  ledger file on the card); the CLI binary itself is now exercised by tests.
- Settings: enabling delete-after-verify works on macOS (the confirm dialog now
  uses the Tauri dialog plugin; `window.confirm` is unimplemented in WKWebView);
  the settings window re-reads the config after the wizard and on focus, so
  saving no longer reverts wizard choices; disabling the cloud mirror no longer
  deletes the keychain password until the change is actually saved; the wizard
  validates the config before storing the credential and warns on insecure
  `http://` URLs; the wizard and Cloud tab share one set of cloud defaults; the
  plaintext-credential warning also covers ids not currently active; keychain
  failures surface inline instead of vanishing; numeric inputs are normalized
  before save; the Advanced tab shows the real config-file path.
- Popover/UI: progress bars have accessible names; `percent()` guards non-finite
  input; the initial state fetch can no longer overwrite a fresher event
  snapshot; form labels without a control association no longer render as
  orphaned `<label>` elements.
- App (Tauri layer): the tray icon is derived from one folded state snapshot
  (a cloud upload finishing mid-offload can no longer flip it to idle or clear
  an error); hard offload errors clear the phantom in-flight progress bar and
  surface in the popover; "Retry failed" updates the badge immediately and
  failed counts survive a restart; state snapshots carry a sequence number so
  an older frame can never overwrite a newer one; I/O-bearing commands run off
  the main thread (no more UI freezes on keychain prompts or slow saves); a
  misconfigured cloud notifies once per distinct error instead of every 5
  seconds; revoked/migrated credentials stop resolving immediately (no restart
  needed); toggling wired ingest applies live; a persistently failing wired
  camera retries at most twice before waiting for a re-plug; the post-upload
  card-delete failure is reported as a warning, not a failed upload.
- App (hardening): a Content-Security-Policy is set and the global Tauri
  object is disabled (the UI uses module imports); upload session ids are
  salted so two machines mirroring the same account cannot collide; retry
  scheduling no longer over-sleeps after a long failed upload.

### Changed

- CI: the workspace is now type-checked on Windows on every push/PR; CI results
  for main-branch commits are no longer cancelled by newer pushes.
- Release: SHA-256 checksum files are attached to release artifacts (builds are
  unsigned, so checksums are the only integrity check); the release-cutting
  instructions point at the real version source (workspace `Cargo.toml`).

## [0.2.0] — 2026-06-08

### Added

- **Wired USB GoPro offload (M4)** — auto-detect a USB-connected GoPro over the
  Open GoPro HTTP API (IP-over-USB), list media, and offload through the same
  verified/atomic/idempotent pipeline, with HTTP-range resume of partial
  downloads and a `wired_ingest` config toggle (Settings → Behavior).
- Deferred camera-delete: with the cloud mirror on `auto`,
  delete-after-verify on a wired camera waits for the upload to finish and reaps
  the file on the next connect — only after re-verifying on-card identity.
- Plaintext-credential migration: the Cloud tab detects a legacy plaintext
  password in `gpbeam.toml` and moves it to the OS keychain in one click
  (keeping the username in the file).
- Inline warning for insecure `http://` cloud URLs in the Cloud tab, mirroring
  the backend's https/loopback-only rule.

### Fixed

- All findings from the 2026-06-08 architecture risk review (H1–H2, M1–M7,
  L1–L5).
- Live-test fixes for wired offload: streaming (not buffered) downloads,
  poller-vs-offload camera contention, live download progress.

### Changed

- Version is single-sourced from the workspace `Cargo.toml` (About tab and
  `gpbeam-cli --version` read it from there).

## [0.1.0] — 2026-06-04

Initial milestone releases (M1–M3), untagged:

- **M1 — core engine:** GoPro card detection + identification, SQLite ledger,
  verified/atomic/resumable/idempotent offload, filename templates, proxy and
  thumbnail filters, low-disk guard, delete-after-verify and auto-eject safety
  flags, headless CLI (`offload` / `watch` / `mirror` / `mirror-status` /
  `retry-cloud`).
- **M2 — Nextcloud cloud mirror:** persisted, resumable upload queue with
  chunked uploads and exponential-backoff retry; keychain credentials.
- **M3 — GUI:** Svelte 5 + Vite tray popover, tabbed settings window, first-run
  wizard, launch-at-login, light/dark theme, `gpbeam://state` snapshot channel.

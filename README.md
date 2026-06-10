# GPBeam

**Plug in a GoPro, footage lands safely on your drive, you glance at a tray icon — done.**

GPBeam is a lightweight cross-platform desktop utility that detects a GoPro — an SD card
in mass-storage mode **or** the camera itself over a USB cable — copies its new media to a
drive you choose, optionally mirrors it to the cloud, and **verifies every byte** —
automatically, from a menu-bar / system-tray footprint that stays out of the way until you
need it.

- **Zero-click capture** — detect a card or a wired camera and start copying, no prompts.
- **Never lose footage** — copies are non-destructive, checksum-verified, resumable, and idempotent.
- **Diminutive** — lives in the tray; the window only appears when you open it.
- **Format-agnostic** — works with current and future GoPro media without code changes.

> Status: **v0.2** — the core engine, Nextcloud cloud mirror, full GUI, and wired USB
> offload (Open GoPro HTTP API) are all implemented. Built and tested on macOS; Windows is
> a first-class target (see [Platform support](#platform-support)). Validation cameras:
> HERO11, Max 2, Mission 1 Pro.

## Features

- **Auto-detect & identify** — recognizes a GoPro card by its `DCIM/###GOPRO/` layout
  and `/MISC/version.txt`, extracting model + serial when available.
- **Wired USB offload** — plug a modern GoPro in by USB (no card reader needed) and GPBeam
  talks to the camera directly over the [Open GoPro](https://gopro.github.io/OpenGoPro/)
  HTTP API: auto-detects it on the IP-over-USB interface, lists its media, and streams
  downloads through the same verified pipeline, resuming partial transfers with HTTP
  range requests. See [Wired USB offload](#wired-usb-offload).
- **Verified, atomic, resumable copy** — streamed copy to a `.part` temp then atomic
  rename, optional **BLAKE3** verification before a file is marked done, and clean resume
  if a card is pulled or the machine sleeps mid-run.
- **Idempotent** — re-plugging the same card or camera copies nothing new (dedup on
  content + camera serial + original name, tracked in a SQLite ledger).
- **Naming, layout & filters** — configurable filename template
  (`{date}`, `{time}`, `{camera}`, `{model}`, `{original}`, `{ext}`; default
  `{date}_{original}`), flat layout, and proxy/sidecar (`.LRV`/`.THM`) skipping.
- **Low-disk guard** — estimates required space before a run and refuses to partially fill
  the drive.
- **Cloud mirror (Nextcloud)** — local-first, then a background, **persisted & resumable**
  upload queue (chunked uploads for large files, exponential-backoff retry). Credentials
  live in the **OS keychain** — and if an older config still carries a plaintext password,
  the Cloud tab flags it and migrates it to the keychain in one click.
- **Safety actions** — opt-in delete-after-verify and auto-eject (both default **off**,
  destructive actions are gated). For a wired camera with the cloud mirror on `auto`,
  deletion is **deferred** until the upload finishes: the file is reaped on the next
  connect, and only after re-checking it is still the very same file on the camera.
- **GUI** — a live tray **popover** (current/last run, byte progress + ETA, cloud-mirror
  progress, pause/resume, retry), a tabbed **settings** window, and a **first-run wizard**.
  Launch-at-login and system light/dark theme included.

## How it works

GPBeam is a single [Tauri 2](https://tauri.app) process split into an always-on Rust core
and a web UI that only spins up for the popover/settings window.

```
crates/gpbeam-core   Pure-Rust engine: device detection, GoPro id/classify, SQLite ledger,
                     verified/atomic copy, scanner+diff, the offload orchestrator, the
                     wired-GoPro module (Open GoPro HTTP client + USB detection + offload),
                     and the async Nextcloud cloud worker (resumable queue + retry).
crates/gpbeam-cli    Headless binary that drives the core (offload / watch / mirror / …).
src-tauri            The Tauri desktop app: tray, command/event bridge, AppState snapshot,
                     keychain-backed credentials, the wired-camera poller, and the
                     long-lived cloud loop.
ui                   Svelte 5 + Vite + TypeScript frontend (popover + settings + wizard),
                     a thin renderer over the Rust AppState pushed on `gpbeam://state`.
```

## Requirements

- **Rust** (stable) and **Cargo**
- **Node.js** 18+ and **npm**
- Tauri 2 system dependencies:
  - **macOS:** Xcode Command Line Tools
  - **Windows:** WebView2 runtime + the MSVC build tools
  - **Linux:** WebKitGTK and the standard Tauri build deps (untested — see
    [Platform support](#platform-support))

## Build & run

Install the frontend dependencies once:

```bash
npm --prefix ui install
```

### The desktop app

With the Tauri CLI (`cargo install tauri-cli`), for hot-reload development:

```bash
cargo tauri dev
```

Or build the frontend and run the bundled binary directly:

```bash
npm --prefix ui run build
cargo run -p gpbeam
```

Produce a distributable bundle:

```bash
cargo tauri build
```

GPBeam runs in the menu bar / system tray. On first launch (no config yet) it opens a short
wizard: pick a destination folder, optionally connect Nextcloud, then it collapses to the tray.

### The CLI (headless)

```bash
cargo run -p gpbeam-cli -- [--version] [--config <path>] [--delete-after-verify] [--auto-eject] <command>
```

| Command | Description |
|---|---|
| `offload <card> <dest>` | Offload a mounted card to `dest` (and mirror if cloud is `auto`/`manual`). |
| `watch <dest>` | Watch for GoPro cards and offload each on plug-in. |
| `mirror <dest>` | Flush the cloud upload queue on demand (any mirror mode). |
| `mirror-status <dest>` | List cloud jobs by state with the pending count. |
| `retry-cloud <dest>` | Re-queue every cloud job that has permanently failed. |

CLI notes:

- The CLI reads only the `[cloud]` table and the two safety keys
  (`delete_after_verify`, `auto_eject`) from `gpbeam.toml`; offload settings (template,
  filters, verify, headroom) use the defaults, and the destination always comes from the
  `<dest>` argument.
- Wired USB offload is a desktop-app feature; the CLI handles mounted cards only.
- The ledger lives at `<dest>/.gpbeam-ledger.sqlite`. (Note: the desktop app keeps its
  ledger at its own bootstrap destination — `$GPBEAM_DEST` or `~/GPBeam` — so the CLI and
  the app track imports separately unless pointed at the same place.)
- Only one process drains the cloud queue at a time (a lock file next to the ledger
  guards it); if the desktop app is already mirroring, `mirror` prints a notice to
  stderr and exits `0` without uploading.
- Exit codes: `0` clean run, `1` runtime or partial failure (some files/uploads failed),
  `2` usage error.

## Configuration

Settings live in `gpbeam.toml` (managed by the GUI, or hand-edited). Resolution order for the
file: `$GPBEAM_CONFIG`, else `<destination>/gpbeam.toml`.

```toml
dest_root         = "/Volumes/videos/GoPro"   # where footage is copied
filename_template = "{date}_{original}"
include_proxies   = false                       # skip .LRV
include_thumbnails = false                      # skip .THM
layout            = "Flat"                       # the only layout today
verify            = true                         # BLAKE3 verify before marking done
space_headroom    = 1073741824                   # keep ≥ 1 GiB free
delete_after_verify = false                      # opt-in, destructive
auto_eject        = false                        # opt-in
wired_ingest      = true                         # offload USB-connected GoPros (Open GoPro API)

[cloud]                                           # optional — Nextcloud mirror
kind          = "nextcloud"
destination_id = "nc1"
base_url      = "https://cloud.example.com"      # https required (http allowed for loopback only)
username      = "alice"
remote_root   = "GoPro"
mirror_mode   = "auto"                            # off | auto | manual
chunk_threshold = 52428800                        # chunk uploads above 50 MiB
max_concurrency = 2
max_attempts    = 8
# tls_ca_pem  = "/path/to/ca.pem"                # optional: trust a custom CA (self-hosted)
```

**Secrets stay out of the config.** The Nextcloud app-password lives in the OS keychain
(macOS Keychain / Windows Credential Manager). A legacy `[credentials.<id>]` table with a
plaintext password is still read as a fallback for older setups — the Cloud tab detects it
and offers a one-click **Move to keychain** migration (which keeps the username in the file
and strips only the password).

Environment variables:

- `GPBEAM_CONFIG` — path to `gpbeam.toml` (highest precedence).
- `GPBEAM_NC_USERNAME` / `GPBEAM_NC_APP_PASSWORD` — Nextcloud credential overrides for
  headless/CI use (precedence: env → keychain → config fallback).
- `GPBEAM_DEST` — destination override for the **desktop app** (the CLI takes `<dest>` as
  an argument).

## Wired USB offload

Modern GoPros expose the Open GoPro HTTP API over IP-over-USB (GoPro Connect): the host
gets an address in `172.20.0.0`–`172.29.255.255` and the camera answers at `.51` on the
same `/24`, port 8080. GPBeam polls for that interface, confirms the camera with a
`/gopro/version` probe, then offloads through the exact same verified/atomic/idempotent
pipeline as a card — including resumable downloads of partially-transferred files.

- Toggle it with the **“Offload a USB-connected GoPro”** checkbox in Settings → Behavior
  (`wired_ingest` in the config).
- **macOS:** the app needs **Local Network** permission (System Settings → Privacy &
  Security) to reach the camera’s USB network endpoint.
- MTP is still not used or required — the camera is driven entirely over HTTP.

## Testing

```bash
cargo test --workspace          # Rust unit + integration tests
npm --prefix ui run test        # frontend (Vitest + @testing-library/svelte)
npm --prefix ui run check       # svelte-check (type/template)
cargo clippy --workspace --all-targets -- -D warnings
```

Cloud and wired-camera tests run against mocked HTTP servers (`wiremock`), so the suite is
fully headless. CI runs the full Rust workspace on macOS and the frontend checks on Ubuntu.

## Platform support

GPBeam ingests footage two ways: **SD / mass-storage mode** (plug the camera in as storage,
or use a card reader) and **wired USB** via the Open GoPro HTTP API. macOS is the primary
development and validation platform (full test suite in CI); Windows is a first-class
target — release installers are built for it and the workspace is type-checked on Windows
in CI, but the test suite is not yet run there. Linux builds are untested (community
territory). The device watcher currently polls removable volumes and USB interfaces;
native DiskArbitration / `WM_DEVICECHANGE` hooks are a future refinement.

## Roadmap

**Done:** auto-detect + multi-camera ID · verified/atomic/resumable/idempotent local offload ·
filename templates + flat layout + proxy skipping · file filters + low-disk guard ·
Nextcloud cloud mirror (persisted resumable queue + retry) · keychain credentials +
plaintext-credential migration · delete-after-verify / auto-eject · wired USB GoPro offload
(Open GoPro HTTP API) with deferred camera-delete · tray popover + settings + first-run
wizard · launch-at-login.

**Planned:** Google Drive backend · multiple named profiles + per-camera binding ·
richer searchable run history · additional destination layouts (by-date / by-session / mirror-card).

## License

[MIT](LICENSE) © 2026 modalmind

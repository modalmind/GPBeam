# GPBeam

**Plug in a GoPro, footage lands safely on your drive, you glance at a tray icon — done.**

GPBeam is a lightweight cross-platform desktop utility that detects when a GoPro is
plugged in (as SD / mass-storage), copies its new media to a drive you choose, optionally
mirrors it to the cloud, and **verifies every byte** — automatically, from a menu-bar /
system-tray footprint that stays out of the way until you need it.

- **Zero-click capture** — detect a GoPro and start copying, no prompts.
- **Never lose footage** — copies are non-destructive, checksum-verified, resumable, and idempotent.
- **Diminutive** — lives in the tray; the window only appears when you open it.
- **Format-agnostic** — works with current and future GoPro media without code changes.

> Status: **v0.1** — the core engine, Nextcloud cloud mirror, and the full GUI are all
> implemented. Built and tested on macOS; Windows is a first-class target (see
> [Platform support](#platform-support)). Validation cameras: HERO11, Max 2, Mission 1 Pro.

## Features

- **Auto-detect & identify** — recognizes a GoPro card by its `DCIM/###GOPRO/` layout
  and `/MISC/version.txt`, extracting model + serial when available.
- **Verified, atomic, resumable copy** — streamed copy to a `.part` temp then atomic
  rename, optional **BLAKE3** verification before a file is marked done, and clean resume
  if a card is pulled or the machine sleeps mid-run.
- **Idempotent** — re-plugging the same card copies nothing new (dedup on content + camera
  serial + original name, tracked in a SQLite ledger).
- **Naming, layout & filters** — configurable filename template
  (`{date}`, `{time}`, `{camera}`, `{model}`, `{original}`, `{ext}`; default
  `{date}_{original}`), flat layout, and proxy/sidecar (`.LRV`/`.THM`) skipping.
- **Low-disk guard** — estimates required space before a run and refuses to partially fill
  the drive.
- **Cloud mirror (Nextcloud)** — local-first, then a background, **persisted & resumable**
  upload queue (chunked uploads for large files, exponential-backoff retry). Credentials
  live in the **OS keychain**, never in the config or database.
- **Safety actions** — opt-in delete-after-verify and auto-eject (both default **off**,
  destructive actions are gated).
- **GUI** — a live tray **popover** (current/last run, byte progress + ETA, cloud-mirror
  progress, pause/resume, retry), a tabbed **settings** window, and a **first-run wizard**.
  Launch-at-login and system light/dark theme included.

## How it works

GPBeam is a single [Tauri 2](https://tauri.app) process split into an always-on Rust core
and a web UI that only spins up for the popover/settings window.

```
crates/gpbeam-core   Pure-Rust engine: device detection, GoPro id/classify, SQLite ledger,
                     verified/atomic copy, scanner+diff, the offload orchestrator, and the
                     async Nextcloud cloud worker (resumable queue + retry).
crates/gpbeam-cli    Headless binary that drives the core (offload / watch / mirror / …).
src-tauri            The Tauri desktop app: tray, command/event bridge, AppState snapshot,
                     keychain-backed credentials, and the long-lived cloud loop.
ui                   Svelte 5 + Vite + TypeScript frontend (popover + settings + wizard),
                     a thin renderer over the Rust AppState pushed on `gpbeam://state`.
```

## Requirements

- **Rust** (stable) and **Cargo**
- **Node.js** 18+ and **npm**
- Tauri 2 system dependencies:
  - **macOS:** Xcode Command Line Tools
  - **Windows:** WebView2 runtime + the MSVC build tools
  - **Linux:** WebKitGTK and the standard Tauri build deps

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
cargo run -p gpbeam-cli -- [--config <path>] [--delete-after-verify] [--auto-eject] <command>
```

| Command | Description |
|---|---|
| `offload <card> <dest>` | Offload a mounted card to `dest` (and mirror if cloud is `auto`/`manual`). |
| `watch <dest>` | Watch for GoPro cards and offload each on plug-in. |
| `mirror <dest>` | Flush the cloud upload queue on demand (any mirror mode). |
| `mirror-status <dest>` | List cloud jobs by state with the pending count. |
| `retry-cloud <dest>` | Re-queue every cloud job that has permanently failed. |

## Configuration

Settings live in `gpbeam.toml` (managed by the GUI, or hand-edited). Resolution order for the
file: `$GPBEAM_CONFIG`, else `<destination>/gpbeam.toml`.

```toml
dest_root         = "/Volumes/videos/GoPro"   # where footage is copied
filename_template = "{date}_{original}"
include_proxies   = false                       # skip .LRV
include_thumbnails = false                      # skip .THM
layout            = "Flat"
verify            = true                         # BLAKE3 verify before marking done
space_headroom    = 1073741824                   # keep ≥ 1 GiB free
delete_after_verify = false                      # opt-in, destructive
auto_eject        = false                        # opt-in

[cloud]                                           # optional — Nextcloud mirror
kind          = "nextcloud"
destination_id = "nc1"
base_url      = "https://cloud.example.com"
username      = "alice"
remote_root   = "GoPro"
mirror_mode   = "auto"                            # off | auto | manual
chunk_threshold = 52428800                        # chunk uploads above 50 MiB
max_concurrency = 2
max_attempts    = 8
```

**Secrets are never stored in the config or DB.** The Nextcloud app-password lives in the OS
keychain (macOS Keychain / Windows Credential Manager). For headless/CI use, the
`GPBEAM_NC_USERNAME` and `GPBEAM_NC_APP_PASSWORD` environment variables override the keychain.
`GPBEAM_DEST` overrides the destination.

## Testing

```bash
cargo test --workspace          # Rust unit + integration tests
npm --prefix ui run test        # frontend (Vitest + @testing-library/svelte)
npm --prefix ui run check       # svelte-check (type/template)
cargo clippy --workspace --all-targets -- -D warnings
```

Cloud tests run against a mocked WebDAV server (`wiremock`), so the suite is fully headless.

## Platform support

Windows + macOS are both first-class targets. GPBeam works in **SD / mass-storage mode**
(plug the camera in as storage, or use a card reader); MTP is out of scope. The device watcher
currently polls removable volumes; native DiskArbitration / `WM_DEVICECHANGE` hooks are a
future refinement.

## Roadmap

**Done:** auto-detect + multi-camera ID · verified/atomic/resumable/idempotent local offload ·
filename templates + flat layout + proxy skipping · file filters + low-disk guard ·
Nextcloud cloud mirror (persisted resumable queue + retry) · keychain credentials ·
delete-after-verify / auto-eject · tray popover + settings + first-run wizard · launch-at-login.

**Planned:** Google Drive backend · multiple named profiles + per-camera binding ·
richer searchable run history · additional destination layouts (by-date / by-session / mirror-card).

## License

[MIT](LICENSE) © 2026 modalmind

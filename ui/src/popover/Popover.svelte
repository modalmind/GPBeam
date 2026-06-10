<script lang="ts">
  import { onMount } from "svelte";
  import { appState, hydrate, subscribeState } from "../lib/store";
  import {
    pauseCloud,
    resumeCloud,
    retryFailedCloud,
    openPath,
    openSettings,
    quit,
  } from "../lib/bindings";
  import type { RunProgress } from "../lib/bindings";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { humanBytes, etaHuman, percent } from "../lib/format";

  // Live application snapshot. Store is the single source of truth (design §4.1).
  $: state = $appState;
  $: run = state.run;
  $: cloud = state.cloud;
  $: lastRun = state.lastRun;

  // ETA: bytes remaining at the observed rate. Recomputed against a ticking clock so it
  // counts down between events. The store carries startedAtUnix + byte totals; the math
  // mirrors AppState::eta_secs on the Rust side but only for display.
  let nowUnix = Math.floor(Date.now() / 1000);
  // Takes (run, now) as args so Svelte's reactive `$: eta` tracks BOTH — otherwise a
  // no-arg call hides the `nowUnix` dependency and the ETA never counts down on the tick.
  function etaSecs(r: RunProgress | null, now: number): number | null {
    if (!r) return null;
    const elapsed = now - r.startedAtUnix;
    if (r.bytesDone <= 0 || elapsed <= 0 || r.bytesDone >= r.bytesTotal) {
      return null;
    }
    const rate = r.bytesDone / elapsed;
    if (rate <= 0) return null;
    return Math.ceil((r.bytesTotal - r.bytesDone) / rate);
  }
  $: eta = etaSecs(run, nowUnix);

  // Reactive (not a no-arg function) so it re-derives whenever `state.status` changes;
  // a `{statusWord()}` call would be computed once and never update after hydrate.
  $: statusWord =
    state.status === "working" ? "working" : state.status === "error" ? "error" : "idle";

  async function onPause() {
    if (cloud.paused) {
      await resumeCloud();
    } else {
      await pauseCloud();
    }
  }

  async function onRetry() {
    await retryFailedCloud();
  }

  async function openDestination() {
    // Opens the configured destination root. The backend resolves the actual path; we
    // pass an empty string as the "default destination" sentinel the command understands.
    await openPath("");
  }

  async function onSettings() {
    // Open the dedicated decorated settings window (NOT navigate this popover's own
    // transparent, frameless webview — doing that rendered settings see-through).
    await openSettings();
    // Dismiss the transient popover once settings are up. Non-fatal if hide is denied.
    getCurrentWindow().hide().catch(() => {});
  }

  async function onQuit() {
    await quit();
  }

  onMount(() => {
    // Listener first, then hydrate: an event snapshot arriving while get_state
    // is in flight must not be clobbered by the (staler) hydrate result. The
    // store also guards this ordering (hydrate drops its result after an event).
    const unsub = subscribeState();
    void hydrate();
    const timer = setInterval(() => {
      nowUnix = Math.floor(Date.now() / 1000);
    }, 1000);
    return () => {
      clearInterval(timer);
      if (typeof unsub === "function") unsub();
    };
  });
</script>

<header class="head">
  <span class="dot" class:working={state.status === "working"} class:error={state.status === "error"} aria-hidden="true"></span>
  <span class="title">GPBeam</span>
  <span class="state-word" data-testid="status-word">{statusWord}</span>
</header>

{#if run}
  <section class="card run-card" data-testid="run-card">
    <div class="run-head">
      <span class="run-file" data-testid="current-file">{run.currentFile ?? "Preparing…"}</span>
      {#if run.model || run.serial}
        <span class="chip" data-testid="device-chip">{run.model ?? "GoPro"}{run.serial ? ` · ${run.serial}` : ""}</span>
      {/if}
    </div>
    <div class="bar" role="progressbar" aria-label="Offload progress" aria-valuemin="0" aria-valuemax="100" aria-valuenow={percent(run.bytesDone, run.bytesTotal)}>
      <div class="bar-fill" style={`width:${percent(run.bytesDone, run.bytesTotal)}%`}></div>
    </div>
    <div class="run-meta">
      <span data-testid="file-count">file {Math.min(run.filesDone + 1, run.filesTotal)} of {run.filesTotal}</span>
      <span data-testid="bytes">{humanBytes(run.bytesDone)} / {humanBytes(run.bytesTotal)}</span>
      <span data-testid="eta">ETA {etaHuman(eta)}</span>
    </div>
  </section>
{:else if lastRun}
  <section class="card summary-card" data-testid="last-run">
    <div class="summary-line">
      Copied {lastRun.copied}, skipped {lastRun.skipped}, failed {lastRun.failed}
    </div>
    <div class="summary-bytes muted">{humanBytes(lastRun.bytes)} transferred</div>
  </section>
{:else}
  <section class="card empty-card muted" data-testid="empty">
    Plug in a GoPro (SD / storage mode) to begin…
  </section>
{/if}

{#if cloud.configured}
  <section class="card cloud-card" data-testid="cloud-card">
    <div class="cloud-head">
      <span class="cloud-title">Cloud mirror</span>
      <span class="cloud-counts" data-testid="cloud-counts">{cloud.pending} pending / {cloud.failed} failed</span>
    </div>
    {#if cloud.uploading}
      <div class="cloud-file" data-testid="cloud-file">{cloud.uploading.file}</div>
      <div class="bar" role="progressbar" aria-label="Upload progress" aria-valuemin="0" aria-valuemax="100" aria-valuenow={percent(cloud.uploading.uploaded, cloud.uploading.total)}>
        <div class="bar-fill cloud" style={`width:${percent(cloud.uploading.uploaded, cloud.uploading.total)}%`}></div>
      </div>
    {/if}
    <div class="cloud-actions">
      <button type="button" data-testid="pause-btn" on:click={onPause}>
        {cloud.paused ? "Resume" : "Pause"}
      </button>
      <button type="button" data-testid="retry-btn" on:click={onRetry} disabled={cloud.failed === 0}>
        Retry failed
      </button>
    </div>
  </section>
{/if}

{#if state.message}
  <p class="message" class:error={state.status === "error"} data-testid="message">{state.message}</p>
{/if}

<footer class="foot">
  <button type="button" class="link" data-testid="open-dest" on:click={openDestination}>Open destination</button>
  <button type="button" class="link" data-testid="open-settings" on:click={onSettings}>Settings…</button>
  <button type="button" class="link" data-testid="quit" on:click={onQuit}>Quit</button>
</footer>

<style>
  :global(html, body) {
    margin: 0;
    height: 100%;
  }
  :global(body) {
    font: 13px/1.4 -apple-system, "Segoe UI", system-ui, sans-serif;
    background: rgba(28, 28, 32, 0.96);
    color: #eee;
    border-radius: 10px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 14px 14px 10px;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: #5a5a60;
    flex: 0 0 auto;
  }
  .dot.working {
    background: #2878dc;
  }
  .dot.error {
    background: #d23c3c;
  }
  .title {
    font-size: 14px;
    font-weight: 600;
  }
  .state-word {
    color: #999;
    margin-left: auto;
    text-transform: lowercase;
  }
  .card {
    margin: 0 14px 10px;
    padding: 10px 12px;
    background: rgba(0, 0, 0, 0.25);
    border-radius: 8px;
  }
  .run-head,
  .cloud-head {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 8px;
  }
  .run-file,
  .cloud-file {
    font: 11px/1.4 ui-monospace, Menlo, monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .cloud-file {
    margin-bottom: 8px;
  }
  .chip {
    margin-left: auto;
    font-size: 11px;
    color: #bbb;
    background: rgba(255, 255, 255, 0.08);
    padding: 2px 7px;
    border-radius: 10px;
    flex: 0 0 auto;
  }
  .bar {
    height: 7px;
    background: rgba(255, 255, 255, 0.1);
    border-radius: 4px;
    overflow: hidden;
  }
  .bar-fill {
    height: 100%;
    background: #2878dc;
    transition: width 0.25s ease;
  }
  .bar-fill.cloud {
    background: #3aa06a;
  }
  .run-meta {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    margin-top: 7px;
    font-size: 11px;
    color: #bbb;
  }
  .cloud-head .cloud-title {
    font-weight: 600;
  }
  .cloud-counts {
    margin-left: auto;
    color: #bbb;
    font-size: 11px;
  }
  .cloud-actions {
    display: flex;
    gap: 8px;
    margin-top: 9px;
  }
  .cloud-actions button {
    flex: 1;
    font: inherit;
    font-size: 12px;
    padding: 5px 0;
    color: #eee;
    background: rgba(255, 255, 255, 0.1);
    border: none;
    border-radius: 6px;
    cursor: pointer;
  }
  .cloud-actions button:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .summary-line {
    font-weight: 600;
  }
  .summary-bytes {
    margin-top: 3px;
    font-size: 11px;
  }
  .empty-card {
    font-size: 12px;
  }
  .message {
    margin: 0 14px 10px;
    font-size: 11px;
    color: #bbb;
  }
  .message.error {
    color: #ec8a8a;
  }
  .muted {
    color: #999;
  }
  .foot {
    display: flex;
    gap: 14px;
    margin-top: auto;
    padding: 10px 14px 14px;
  }
  .foot .link {
    font: inherit;
    font-size: 12px;
    color: #6fb0ff;
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
  }
  .foot .link:hover {
    text-decoration: underline;
  }
</style>

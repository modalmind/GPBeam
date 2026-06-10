<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import type { ConfigView } from '../lib/bindings';
  import {
    getConfig,
    saveConfig,
    isFirstRun,
    getConfigPath,
    clearNextcloudCredentials,
  } from '../lib/bindings';
  import { defaultCloudView } from './wizard_view';
  import Wizard from './Wizard.svelte';
  import DestinationTab from './tabs/DestinationTab.svelte';
  import BehaviorTab from './tabs/BehaviorTab.svelte';
  import CloudTab from './tabs/CloudTab.svelte';
  import HistoryTab from './tabs/HistoryTab.svelte';
  import AdvancedTab from './tabs/AdvancedTab.svelte';
  import AboutTab from './tabs/AboutTab.svelte';

  // Placeholder until get_config_path resolves the real on-disk location.
  let configPath = 'gpbeam.toml';

  type TabId = 'destination' | 'behavior' | 'cloud' | 'history' | 'advanced' | 'about';
  const TABS: { id: TabId; label: string }[] = [
    { id: 'destination', label: 'Destination' },
    { id: 'behavior', label: 'Behavior' },
    { id: 'cloud', label: 'Cloud' },
    { id: 'history', label: 'History' },
    { id: 'advanced', label: 'Advanced' },
    { id: 'about', label: 'About' },
  ];

  let active: TabId = 'destination';
  let view: ConfigView | null = null;
  let status: 'idle' | 'saving' = 'idle';
  let notice: { kind: 'ok' | 'err'; text: string } | null = null;
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;

  // First-run gate: null = undecided (render nothing yet), true = wizard, false = tabs.
  let firstRun: boolean | null = null;

  // Default destination offered by the wizard's folder step.
  const defaultDest = '~/GPBeam';

  // JSON snapshot of the persisted config backing `view`, used to detect
  // unsaved edits so focus-rehydration never clobbers in-progress changes.
  let loadedSnapshot: string | null = null;
  // destinationId of the cloud section in the *persisted* config — the key the
  // keychain credential lives under. Clearing it is deferred to a successful
  // save with cloud removed (the app-password is unrecoverable once cleared).
  let savedCloudId: string | null = null;

  let destroyed = false;
  let unlistenFocus: (() => void) | null = null;

  async function rehydrate() {
    const v = await getConfig();
    view = v;
    loadedSnapshot = JSON.stringify(v);
    savedCloudId = v.cloud?.destinationId ?? null;
  }

  function isDirty(): boolean {
    return view !== null && JSON.stringify(view) !== loadedSnapshot;
  }

  // The settings window is created once and only hidden on close, so onMount
  // runs once per app lifetime. Refresh stale state (config saved by the
  // wizard, plaintextCredentialIds, hasPassword) whenever the window comes
  // back — but never over unsaved edits.
  async function maybeRehydrate() {
    if (firstRun !== false) return;
    if (view !== null && isDirty()) return;
    await rehydrate();
  }

  onMount(async () => {
    firstRun = await isFirstRun();
    await rehydrate();
    try {
      const p = await getConfigPath();
      if (typeof p === 'string' && p !== '') configPath = p;
    } catch {
      // Backend unavailable: keep the 'gpbeam.toml' placeholder.
    }
    const unlisten = await getCurrentWindow().listen('tauri://focus', () => {
      void maybeRehydrate();
    });
    if (destroyed) unlisten();
    else unlistenFocus = unlisten;
  });

  onDestroy(() => {
    destroyed = true;
    unlistenFocus?.();
    unlistenFocus = null;
  });

  async function onWizardDone() {
    // The window has hidden itself; flip the gate so a re-show lands on the
    // tabs, and refetch — the wizard saved a fresh config via complete_wizard,
    // so the launch-time snapshot is stale (Save would silently revert it).
    firstRun = false;
    await rehydrate();
  }

  // Blank/partial number inputs bind null/NaN, which Rust u64/usize reject in
  // serde with a cryptic invoke error — normalize to defaults and clamp before
  // shipping the view.
  function sanitizeCloudNumbers(v: ConfigView) {
    if (!v.cloud) return;
    const d = defaultCloudView();
    const num = (x: unknown): number | null =>
      typeof x === 'number' && Number.isFinite(x) ? x : null;
    v.cloud.chunkThreshold = Math.max(0, Math.floor(num(v.cloud.chunkThreshold) ?? d.chunkThreshold));
    v.cloud.maxConcurrency = Math.max(1, Math.floor(num(v.cloud.maxConcurrency) ?? d.maxConcurrency));
    v.cloud.maxAttempts = Math.max(1, Math.floor(num(v.cloud.maxAttempts) ?? d.maxAttempts));
  }

  async function onSave() {
    if (!view) return;
    status = 'saving';
    notice = null;
    if (noticeTimer) clearTimeout(noticeTimer);
    sanitizeCloudNumbers(view);
    view = view; // re-render the sanitized numbers
    try {
      await saveConfig(view);
      // Only now — after the removal is actually persisted — is it safe to
      // drop the keychain credential for a cloud section this save removed.
      const removedCloudId = view.cloud === null ? savedCloudId : null;
      savedCloudId = view.cloud?.destinationId ?? null;
      loadedSnapshot = JSON.stringify(view);
      if (removedCloudId !== null) {
        try {
          await clearNextcloudCredentials(removedCloudId);
        } catch (e) {
          notice = {
            kind: 'err',
            text: `Saved, but removing the keychain credential failed: ${typeof e === 'string' ? e : (e as Error)?.message ?? e}`,
          };
          return;
        }
      }
      notice = { kind: 'ok', text: 'Saved.' };
      // Auto-dismiss the success toast after a moment; errors stay until next save.
      noticeTimer = setTimeout(() => { notice = null; }, 2500);
    } catch (e) {
      notice = { kind: 'err', text: typeof e === 'string' ? e : (e as Error)?.message ?? 'Save failed.' };
    } finally {
      status = 'idle';
    }
  }
</script>

{#if firstRun === true}
  <Wizard {defaultDest} on:done={onWizardDone} />
{:else if firstRun === false}
<div class="settings">
  <nav class="sidebar">
    {#each TABS as t (t.id)}
      <button
        type="button"
        class="tab"
        class:active={active === t.id}
        aria-current={active === t.id ? 'page' : undefined}
        on:click={() => (active = t.id)}
      >{t.label}</button>
    {/each}
  </nav>

  <main class="pane">
    {#if view}
      {#if active === 'destination'}<DestinationTab {view} />
      {:else if active === 'behavior'}<BehaviorTab {view} />
      {:else if active === 'cloud'}<CloudTab {view} />
      {:else if active === 'history'}<HistoryTab />
      {:else if active === 'advanced'}<AdvancedTab {configPath} destRoot={view.destRoot} />
      {:else if active === 'about'}<AboutTab />
      {/if}
    {:else}
      <p class="muted">Loading…</p>
    {/if}

    {#if active !== 'history' && active !== 'about'}
      <footer class="actions">
        <button type="button" class="primary" on:click={onSave} disabled={status === 'saving' || !view}>Save</button>
        {#if notice}
          <span class="notice" class:err={notice.kind === 'err'}>{notice.text}</span>
        {/if}
      </footer>
    {/if}
  </main>
</div>
{/if}

<style>
  :root { color-scheme: light dark; }
  .settings { display: grid; grid-template-columns: 160px 1fr; height: 100vh; font: 14px/1.5 -apple-system, system-ui, sans-serif; }
  .sidebar { border-right: 1px solid rgba(127,127,127,0.2); padding: 12px 8px; display: flex; flex-direction: column; gap: 2px; }
  .tab { text-align: left; background: none; border: none; padding: 8px 12px; border-radius: 6px; cursor: pointer; font: inherit; color: inherit; }
  .tab:hover { background: rgba(127,127,127,0.12); }
  .tab.active { background: rgba(40,120,220,0.18); font-weight: 600; }
  .pane { padding: 20px 24px; overflow: auto; position: relative; }
  .actions { margin-top: 20px; display: flex; align-items: center; gap: 12px; }
  .primary { padding: 6px 16px; }
  .notice { color: #2e9e54; font-size: 13px; }
  .notice.err { color: #d23c3c; }
  .muted { color: #888; }
</style>

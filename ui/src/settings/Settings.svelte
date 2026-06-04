<script lang="ts">
  import { onMount } from 'svelte';
  import type { ConfigView } from '../lib/bindings';
  import { getConfig, saveConfig, isFirstRun } from '../lib/bindings';
  import Wizard from './Wizard.svelte';
  import DestinationTab from './tabs/DestinationTab.svelte';
  import BehaviorTab from './tabs/BehaviorTab.svelte';
  import CloudTab from './tabs/CloudTab.svelte';
  import HistoryTab from './tabs/HistoryTab.svelte';
  import AdvancedTab from './tabs/AdvancedTab.svelte';
  import AboutTab from './tabs/AboutTab.svelte';

  export let configPath = 'gpbeam.toml';
  export let version = '0.3.0';

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

  onMount(async () => {
    firstRun = await isFirstRun();
    view = await getConfig();
  });

  function onWizardDone() {
    // The window hides itself; flip the gate so a re-show lands on the tabs.
    firstRun = false;
  }

  async function onSave() {
    if (!view) return;
    status = 'saving';
    notice = null;
    if (noticeTimer) clearTimeout(noticeTimer);
    try {
      await saveConfig(view);
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
      {:else if active === 'about'}<AboutTab {version} />
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

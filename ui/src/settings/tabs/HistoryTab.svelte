<script lang="ts">
  import { onMount } from 'svelte';
  import type { HistoryRow } from '../../lib/bindings';
  import { getHistory, revealPath } from '../../lib/bindings';
  import { humanBytes } from '../../lib/format';

  let rows: HistoryRow[] = [];
  let loaded = false;

  onMount(async () => {
    rows = await getHistory(50);
    loaded = true;
  });

  function fmtTime(iso: string): string {
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
  }
</script>

<section>
  <h2>History</h2>

  {#if loaded && rows.length === 0}
    <p class="muted">No transfers yet.</p>
  {:else}
    <table>
      <thead>
        <tr><th>Name</th><th>Size</th><th>Copied</th><th>Cloud</th><th></th></tr>
      </thead>
      <tbody>
        {#each rows as r (r.destPath)}
          <tr>
            <td class="name">{r.name}</td>
            <td>{humanBytes(r.size)}</td>
            <td>{fmtTime(r.copiedAt)}</td>
            <td>{r.cloudStatus ?? '—'}</td>
            <td><button type="button" on:click={() => revealPath(r.destPath)}>Reveal</button></td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  section { max-width: 680px; }
  h2 { font-size: 15px; margin: 0 0 8px; }
  .muted { color: #888; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { text-align: left; padding: 6px 8px; border-bottom: 1px solid rgba(127,127,127,0.2); }
  .name { font-family: ui-monospace, Menlo, monospace; }
</style>

<script lang="ts">
  import { onMount } from "svelte";
  import { appState, hydrate, subscribeState } from "../lib/store";

  let unsubscribe: (() => void) | undefined;

  onMount(() => {
    unsubscribe = subscribeState();
    void hydrate();
    return () => unsubscribe?.();
  });
</script>

<main>
  <h1>GPBeam <span class="state">{$appState.status}</span></h1>
  {#if $appState.message}
    <p class="msg">{$appState.message}</p>
  {:else}
    <p class="muted">Plug in a GoPro (SD / storage mode) to begin…</p>
  {/if}
</main>

<style>
  main { display: flex; flex-direction: column; gap: 8px; }
  h1 { font-size: 14px; margin: 0; display: flex; align-items: center; gap: 8px; }
  .state { color: #999; font-weight: 400; }
  .muted { color: #999; margin: 0; }
  .msg { margin: 0; }
</style>

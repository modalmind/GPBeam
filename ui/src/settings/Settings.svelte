<script lang="ts">
  import { onMount } from "svelte";
  import { hydrate } from "../lib/store";

  onMount(() => {
    void hydrate();
  });
</script>

<main>
  <h1>GPBeam Settings</h1>
  <p class="muted">Settings UI is wired in a later M3 phase.</p>
</main>

<style>
  main { padding: 28px; }
  h1 { font-size: 18px; margin: 0 0 6px; }
  .muted { color: #888; margin: 0; }
</style>

<script lang="ts">
  import { onMount } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';

  // Live app version from the Tauri bundle — single source of truth (the Rust
  // crate version, inherited from the Cargo workspace). No hardcoded fallback.
  let version = '';
  onMount(async () => {
    try {
      version = await getVersion();
    } catch {
      version = 'unknown';
    }
  });
</script>

<section>
  <h2>About</h2>
  <p><strong>GPBeam</strong> <span class="muted">v{version}</span></p>
  <p>Auto-offloads your GoPro to a drive the moment you plug it in.</p>
</section>

<style>
  section { max-width: 560px; }
  h2 { font-size: 15px; margin: 0 0 8px; }
  .muted { color: #888; }
</style>

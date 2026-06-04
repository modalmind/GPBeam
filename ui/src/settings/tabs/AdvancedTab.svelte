<script lang="ts">
  import { openPath } from '../../lib/bindings';

  export let configPath: string;
  export let destRoot: string;
</script>

<section>
  <h2>Advanced</h2>
  <p><strong>Config file</strong></p>
  <p><code>{configPath}</code></p>
  <p>
    <button type="button" on:click={() => openPath(destRoot)}>Open destination folder</button>
  </p>
</section>

<style>
  section { max-width: 560px; }
  h2 { font-size: 15px; margin: 0 0 8px; }
  code { background: rgba(127,127,127,0.18); padding: 2px 6px; border-radius: 4px; word-break: break-all; }
</style>

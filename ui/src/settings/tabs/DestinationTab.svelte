<script lang="ts">
  import type { ConfigView } from '../../lib/bindings';
  import { pickFolder } from '../../lib/bindings';
  import Field from '../../lib/Field.svelte';

  export let view: ConfigView;

  async function choose() {
    const picked = await pickFolder();
    if (picked) view.destRoot = picked;
  }
</script>

<section>
  <h2>Destination</h2>

  <Field label="Destination folder" htmlFor="dest-root"
    help="Footage is copied here, organized by date.">
    <input id="dest-root" aria-label="Destination folder" type="text" bind:value={view.destRoot} />
    <button type="button" on:click={choose}>Choose…</button>
  </Field>

  <Field label="Filename template" htmlFor="tpl"
    help="Tokens: {'{'}date{'}'}, {'{'}original{'}'}, {'{'}model{'}'}, {'{'}serial{'}'}. Default {'{'}date{'}'}_{'{'}original{'}'}.">
    <input id="tpl" aria-label="Filename template" type="text" bind:value={view.filenameTemplate} />
  </Field>

  <Field label="Include proxies" htmlFor="proxies">
    <label class="check">
      <input id="proxies" type="checkbox" aria-label="Include proxy files (.LRV)" bind:checked={view.includeProxies} />
      Include proxy files (.LRV)
    </label>
  </Field>

  <Field label="Include thumbnails" htmlFor="thumbs">
    <label class="check">
      <input id="thumbs" type="checkbox" aria-label="Include thumbnail files (.THM)" bind:checked={view.includeThumbnails} />
      Include thumbnail files (.THM)
    </label>
  </Field>

  <Field label="Layout" help="More layouts arrive in a later release.">
    <select disabled aria-label="Layout">
      <option>Flat</option>
    </select>
  </Field>
</section>

<style>
  section { max-width: 560px; }
  h2 { font-size: 15px; margin: 0 0 8px; }
  input[type='text'] { flex: 1; min-width: 220px; }
  .check { display: flex; align-items: center; gap: 6px; font-weight: 400; }
</style>

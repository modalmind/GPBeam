<script lang="ts">
  import type { ConfigView, CloudView } from '../../lib/bindings';
  import {
    setNextcloudCredentials,
    clearNextcloudCredentials,
    migratePlaintextCredentials,
  } from '../../lib/bindings';
  import { isInsecureHttpUrl } from '../../lib/url';
  import Field from '../../lib/Field.svelte';

  export let view: ConfigView;

  let pendingPassword = '';

  function defaultCloud(): CloudView {
    return {
      destinationId: 'nc1',
      baseUrl: '',
      username: '',
      remoteRoot: 'GoPro',
      mirrorMode: 'off',
      chunkThreshold: 52428800,
      maxConcurrency: 2,
      maxAttempts: 8,
      hasPassword: false,
    };
  }

  async function onEnableToggle(e: Event) {
    const on = (e.currentTarget as HTMLInputElement).checked;
    if (on) {
      if (!view.cloud) view.cloud = defaultCloud();
    } else if (view.cloud) {
      const id = view.cloud.destinationId;
      view.cloud = null;
      pendingPassword = '';
      await clearNextcloudCredentials(id);
    }
  }

  async function savePassword() {
    if (!view.cloud || !pendingPassword) return;
    await setNextcloudCredentials(view.cloud.destinationId, pendingPassword);
    view.cloud.hasPassword = true;
    pendingPassword = '';
  }

  async function migrateToKeychain() {
    if (!view.cloud) return;
    const id = view.cloud.destinationId;
    await migratePlaintextCredentials(id);
    view.plaintextCredentialIds = (view.plaintextCredentialIds ?? []).filter((x) => x !== id);
    view.cloud.hasPassword = true;
  }
</script>

<section>
  <h2>Cloud (Nextcloud)</h2>

  <Field label="Enable mirroring">
    <label class="check">
      <input
        type="checkbox"
        aria-label="Enable Nextcloud mirroring"
        checked={!!view.cloud}
        on:change={onEnableToggle}
      />
      Mirror verified files to Nextcloud
    </label>
  </Field>

  {#if view.cloud}
    {#if (view.plaintextCredentialIds ?? []).includes(view.cloud.destinationId)}
      <div class="warn" role="alert">
        ⚠ Your Nextcloud password is stored in plain text in <code>gpbeam.toml</code>.
        Anyone who can read that file — or a synced/removable copy of it — can see it.
        <button type="button" on:click={migrateToKeychain}>Move to keychain</button>
      </div>
    {/if}

    <Field label="Base URL" htmlFor="nc-url" help="e.g. https://cloud.example.com">
      <input id="nc-url" aria-label="Base URL" type="text" bind:value={view.cloud.baseUrl} />
    </Field>
    {#if isInsecureHttpUrl(view.cloud.baseUrl)}
      <p class="warn-inline" role="alert">
        ⚠ Plain http sends your password and footage unencrypted. Use https:// —
        http is allowed only for localhost.
      </p>
    {/if}

    <Field label="Username" htmlFor="nc-user">
      <input id="nc-user" aria-label="Username" type="text" bind:value={view.cloud.username} />
    </Field>

    <Field label="App password" htmlFor="nc-pw"
      help="Stored in the OS keychain — never written to the config file.">
      <input id="nc-pw" aria-label="App password" type="password" bind:value={pendingPassword} placeholder="••••••••" />
      <button type="button" on:click={savePassword}>Save password</button>
      {#if view.cloud.hasPassword}<span class="saved">Saved</span>{/if}
    </Field>

    <Field label="Remote root" htmlFor="nc-root">
      <input id="nc-root" aria-label="Remote root" type="text" bind:value={view.cloud.remoteRoot} />
    </Field>

    <Field label="Mirror mode" htmlFor="nc-mode">
      <select id="nc-mode" aria-label="Mirror mode" bind:value={view.cloud.mirrorMode}>
        <option value="off">Off</option>
        <option value="auto">Auto</option>
        <option value="manual">Manual</option>
      </select>
    </Field>

    <details class="advanced">
      <summary>Advanced</summary>
      <Field label="Chunk threshold (bytes)" htmlFor="nc-chunk">
        <input id="nc-chunk" aria-label="Chunk threshold" type="number" min="0" bind:value={view.cloud.chunkThreshold} />
      </Field>
      <Field label="Max concurrency" htmlFor="nc-conc">
        <input id="nc-conc" aria-label="Max concurrency" type="number" min="1" bind:value={view.cloud.maxConcurrency} />
      </Field>
      <Field label="Max attempts" htmlFor="nc-att">
        <input id="nc-att" aria-label="Max attempts" type="number" min="1" bind:value={view.cloud.maxAttempts} />
      </Field>
    </details>
  {/if}
</section>

<style>
  section { max-width: 560px; }
  h2 { font-size: 15px; margin: 0 0 8px; }
  .check { display: flex; align-items: center; gap: 6px; font-weight: 400; }
  .saved { color: #2e9e54; font-size: 12px; }
  .warn { background: #fff4e5; border: 1px solid #e0a96d; border-radius: 6px;
          padding: 8px 10px; margin: 8px 0; font-size: 12px; }
  .warn button { margin-left: 6px; }
  .warn-inline { color: #b15c00; font-size: 12px; margin: 4px 0 0; }
  .advanced { margin-top: 10px; }
  input[type='text'], input[type='password'] { flex: 1; min-width: 220px; }
  input[type='number'] { width: 140px; }
</style>

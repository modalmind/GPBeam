<script lang="ts">
  import { onMount } from 'svelte';
  import { ask } from '@tauri-apps/plugin-dialog';
  import type { ConfigView } from '../../lib/bindings';
  import { getAutostart, setAutostart } from '../../lib/bindings';
  import { bytesToGiB, giBToBytes } from '../../lib/format';
  import Field from '../../lib/Field.svelte';

  export let view: ConfigView;

  let headroomGiB = bytesToGiB(view.spaceHeadroom);
  let autostart = false;
  let autostartError: string | null = null;

  onMount(async () => {
    autostart = await getAutostart();
  });

  function onHeadroomInput(e: Event) {
    const raw = parseFloat((e.currentTarget as HTMLInputElement).value);
    headroomGiB = Number.isFinite(raw) ? raw : 0;
    view.spaceHeadroom = giBToBytes(headroomGiB);
  }

  async function onDeleteToggle(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    // `wantOn` is the value the user is trying to move *to* (the platform toggles
    // the checkbox before dispatching click).
    const wantOn = !view.deleteAfterVerify;
    if (wantOn) {
      // Must be the dialog plugin, NOT window.confirm(): wry's WKWebView ships no
      // runJavaScriptConfirmPanelWithMessage handler, so confirm() always returns
      // false on macOS and the feature could never be enabled. A failing dialog
      // counts as declined.
      let ok = false;
      try {
        ok = await ask(
          'Delete originals from the card after a verified copy? ' +
            'Files removed from the card cannot be recovered.',
          { title: 'GPBeam', kind: 'warning' }
        );
      } catch {
        ok = false;
      }
      view.deleteAfterVerify = ok;
    } else {
      view.deleteAfterVerify = false;
    }
    // We own the DOM checkbox (uncontrolled-with-handler via `on:click|preventDefault`),
    // so reconcile it to the resolved value. Do it both now and after the event fully
    // settles, since the platform restores the pre-click state post-dispatch.
    const resolved = view.deleteAfterVerify;
    input.checked = resolved;
    queueMicrotask(() => {
      input.checked = resolved;
    });
  }

  async function onAutostartToggle(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const next = input.checked;
    const prev = autostart;
    autostart = next;
    autostartError = null;
    try {
      await setAutostart(next);
    } catch (err) {
      // Revert the optimistic flip and surface the failure inline.
      autostart = prev;
      input.checked = prev;
      autostartError =
        typeof err === 'string' ? err : ((err as Error)?.message ?? 'Could not update launch-at-login.');
    }
  }
</script>

<section>
  <h2>Behavior</h2>

  <Field label="Verify">
    <label class="check">
      <input type="checkbox" aria-label="Verify each copied file" bind:checked={view.verify} />
      Verify each copied file
    </label>
  </Field>

  <Field label="Delete after verify" help="Frees the card automatically once a copy is verified.">
    <label class="check">
      <input
        type="checkbox"
        aria-label="Delete files from card after verify"
        checked={view.deleteAfterVerify}
        on:click|preventDefault={onDeleteToggle}
      />
      Delete files from card after verify
    </label>
  </Field>

  <Field label="Auto-eject">
    <label class="check">
      <input type="checkbox" aria-label="Eject the card when the run completes" bind:checked={view.autoEject} />
      Eject the card when the run completes
    </label>
  </Field>

  <Field label="USB GoPro" help="Automatically offload a GoPro connected over USB (Open GoPro API).">
    <label class="check">
      <input type="checkbox" aria-label="Offload a USB-connected GoPro" bind:checked={view.wiredIngest} />
      Offload a USB-connected GoPro
    </label>
  </Field>

  <Field label="Low-disk headroom (GiB)" htmlFor="headroom"
    help="Refuse to start a run unless this much free space remains on the destination.">
    <input
      id="headroom"
      aria-label="Low-disk headroom (GiB)"
      type="number"
      min="0"
      step="0.5"
      value={headroomGiB}
      on:input={onHeadroomInput}
    />
  </Field>

  <Field label="Launch at login" help="Start GPBeam automatically when you sign in.">
    <label class="check">
      <input type="checkbox" aria-label="Launch at login" checked={autostart} on:change={onAutostartToggle} />
      Launch at login
    </label>
    {#if autostartError}
      <p class="error" role="alert">{autostartError}</p>
    {/if}
  </Field>
</section>

<style>
  section { max-width: 560px; }
  h2 { font-size: 15px; margin: 0 0 8px; }
  .check { display: flex; align-items: center; gap: 6px; font-weight: 400; }
  input[type='number'] { width: 100px; }
  .error { color: #d23c3c; font-size: 12px; margin: 4px 0 0; }
</style>

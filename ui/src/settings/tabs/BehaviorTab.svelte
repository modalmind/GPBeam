<script lang="ts">
  import { onMount } from 'svelte';
  import type { ConfigView } from '../../lib/bindings';
  import { getAutostart, setAutostart } from '../../lib/bindings';
  import { bytesToGiB, giBToBytes } from '../../lib/format';
  import Field from '../../lib/Field.svelte';

  export let view: ConfigView;

  let headroomGiB = bytesToGiB(view.spaceHeadroom);
  let autostart = false;

  onMount(async () => {
    autostart = await getAutostart();
  });

  function onHeadroomInput(e: Event) {
    const raw = parseFloat((e.currentTarget as HTMLInputElement).value);
    headroomGiB = Number.isFinite(raw) ? raw : 0;
    view.spaceHeadroom = giBToBytes(headroomGiB);
  }

  function onDeleteToggle(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    // `wantOn` is the value the user is trying to move *to* (the platform toggles
    // the checkbox before dispatching click).
    const wantOn = !view.deleteAfterVerify;
    if (wantOn) {
      const ok = window.confirm(
        'Delete originals from the card after a verified copy? ' +
          'Files removed from the card cannot be recovered.'
      );
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
    const next = (e.currentTarget as HTMLInputElement).checked;
    autostart = next;
    await setAutostart(next);
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
  </Field>
</section>

<style>
  section { max-width: 560px; }
  h2 { font-size: 15px; margin: 0 0 8px; }
  .check { display: flex; align-items: center; gap: 6px; font-weight: 400; }
  input[type='number'] { width: 100px; }
</style>

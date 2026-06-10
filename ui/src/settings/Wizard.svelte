<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import {
    pickFolder,
    completeWizard,
    setNextcloudCredentials,
  } from "../lib/bindings";
  import { defaultConfigView, withCloud, buildCloudView } from "./wizard_view";
  import { isInsecureHttpUrl } from "../lib/url";

  /** The default destination offered on the folder step (e.g. "~/GPBeam"). */
  export let defaultDest: string;

  const dispatch = createEventDispatcher<{ done: void }>();

  type Step = "welcome" | "folder" | "cloud";
  let step: Step = "welcome";

  // Folder step state. `dest` is pre-filled with the default, but `folderChosen`
  // gates "Next": the user must actively pick (or accept) a folder via the picker.
  let dest = defaultDest;
  let folderChosen = false;

  // Cloud step state (raw fields).
  let baseUrl = "";
  let username = "";
  let appPassword = "";
  let remoteRoot = "GoPro";
  let mirrorMode: "off" | "auto" | "manual" = "auto";

  let busy = false;
  let error: string | null = null;

  async function choose() {
    const picked = await pickFolder();
    if (picked) {
      dest = picked;
      folderChosen = true;
    }
  }

  function gotoWelcome() {
    step = "welcome";
  }
  function gotoFolder() {
    step = "folder";
  }
  function gotoCloud() {
    step = "cloud";
  }

  async function finish(includeCloud: boolean) {
    if (busy) return;
    busy = true;
    error = null;
    try {
      let view = defaultConfigView(dest);
      let cloud = null as ReturnType<typeof buildCloudView>;
      if (includeCloud) {
        cloud = buildCloudView({
          baseUrl,
          username,
          appPassword,
          remoteRoot,
          mirrorMode,
        });
        view = withCloud(view, cloud);
      }
      // Validate/save the config FIRST so a rejected config never leaves an
      // orphaned secret in the keychain.
      await completeWizard(view);
      if (cloud && appPassword.trim() !== "") {
        await setNextcloudCredentials(cloud.destinationId, appPassword);
      }
      // Hide before dispatching done: if hide rejects (e.g. a missing
      // capability), the error must land on a still-mounted component where
      // the user can see it — `done` swaps Settings over to the tabs.
      await getCurrentWindow().hide();
      dispatch("done");
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="wizard">
  {#if step === "welcome"}
    <section class="step">
      <h1>Welcome to GPBeam</h1>
      <p class="muted">
        GPBeam auto-offloads your GoPro footage the moment you plug it in. Let's
        pick where your footage should go.
      </p>
      <div class="actions">
        <button type="button" on:click={gotoFolder}>Get started</button>
      </div>
    </section>
  {:else if step === "folder"}
    <section class="step">
      <h1>Choose a destination</h1>
      <p class="muted">Footage will be copied into this folder.</p>
      <div class="row">
        <input
          type="text"
          aria-label="Destination folder"
          bind:value={dest}
          readonly
        />
        <button type="button" on:click={choose}>Choose folder…</button>
      </div>
      <div class="actions">
        <button type="button" class="ghost" on:click={gotoWelcome}>Back</button>
        <button type="button" on:click={gotoCloud} disabled={!folderChosen}>
          Next
        </button>
      </div>
    </section>
  {:else}
    <section class="step">
      <h1>Connect Nextcloud (optional)</h1>
      <p class="muted">
        Mirror your footage to Nextcloud, or skip this and set it up later in
        Settings.
      </p>
      <label>
        <span>Base URL</span>
        <input type="text" bind:value={baseUrl} placeholder="https://cloud.example.com" />
      </label>
      {#if isInsecureHttpUrl(baseUrl)}
        <p class="warn-inline" role="alert">
          ⚠ Plain http sends your password and footage unencrypted. Use https:// —
          http is allowed only for localhost.
        </p>
      {/if}
      <label>
        <span>Username</span>
        <input type="text" bind:value={username} />
      </label>
      <label>
        <span>App password</span>
        <input type="password" bind:value={appPassword} />
      </label>
      <label>
        <span>Remote folder</span>
        <input type="text" bind:value={remoteRoot} />
      </label>
      <label>
        <span>Mirror mode</span>
        <select bind:value={mirrorMode}>
          <option value="off">Off</option>
          <option value="auto">Auto</option>
          <option value="manual">Manual</option>
        </select>
      </label>
      {#if error}<p class="error">{error}</p>{/if}
      <div class="actions">
        <button type="button" class="ghost" on:click={gotoFolder} disabled={busy}>Back</button>
        <button type="button" class="ghost" on:click={() => finish(false)} disabled={busy}>
          Skip
        </button>
        <button type="button" on:click={() => finish(true)} disabled={busy}>Finish</button>
      </div>
    </section>
  {/if}
</div>

<style>
  .wizard {
    padding: 24px;
    max-width: 520px;
    margin: 0 auto;
  }
  .step h1 {
    font-size: 18px;
    margin: 0 0 8px;
  }
  .muted {
    color: #888;
  }
  .row {
    display: flex;
    gap: 8px;
    margin: 12px 0;
  }
  .row input {
    flex: 1;
  }
  label {
    display: block;
    margin: 10px 0;
  }
  label span {
    display: block;
    font-size: 12px;
    color: #888;
    margin-bottom: 3px;
  }
  label input,
  label select {
    width: 100%;
    box-sizing: border-box;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 20px;
  }
  .ghost {
    background: transparent;
  }
  .error {
    color: #d23c3c;
  }
  .warn-inline {
    color: #b15c00;
    font-size: 12px;
    margin: 4px 0 0;
  }
</style>

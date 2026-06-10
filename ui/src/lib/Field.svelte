<script lang="ts">
  export let label: string;
  export let help: string | undefined = undefined;
  export let htmlFor: string | undefined = undefined;
</script>

<div class="field">
  {#if htmlFor}
    <label class="field-label" for={htmlFor}>{label}</label>
  {:else}
    <!-- No control id to point at: a <label> here would be an orphan that screen
         readers announce as unassociated. Render a plain span with the same styling. -->
    <span class="field-label">{label}</span>
  {/if}
  <div class="field-control">
    <slot />
  </div>
  {#if help}
    <p class="field-help">{help}</p>
  {/if}
</div>

<style>
  .field {
    display: grid;
    grid-template-columns: 180px 1fr;
    grid-template-areas: 'label control' '. help';
    column-gap: 12px;
    row-gap: 4px;
    align-items: center;
    padding: 8px 0;
  }
  .field-label { grid-area: label; font-weight: 500; }
  .field-control { grid-area: control; display: flex; align-items: center; gap: 8px; }
  .field-help { grid-area: help; margin: 0; font-size: 12px; color: #888; }
</style>

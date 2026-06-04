import { mount } from 'svelte';
import Settings from './Settings.svelte';

const app = mount(Settings, {
  // Svelte 5 mount target; tests assert this exact getElementById call.
  target: document.getElementById('app') as HTMLElement,
});

export default app;

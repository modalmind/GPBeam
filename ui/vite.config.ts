/// <reference types="vitest/config" />
import { defineConfig } from "vitest/config";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { svelteTesting } from "@testing-library/svelte/vite";
import { resolve } from "node:path";

// Vite multi-page app for the Tauri shell: two HTML entry points
// (tray popover + settings window). Output goes to ui/dist, which
// tauri.conf.json points frontendDist at.
export default defineConfig({
  plugins: [svelte(), svelteTesting()],
  // Tauri expects a fixed dev port; fail loudly instead of hopping ports.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        popover: resolve(__dirname, "popover.html"),
        settings: resolve(__dirname, "settings.html"),
      },
    },
  },
  test: {
    globals: true,
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.{test,spec}.{ts,js}"],
  },
});

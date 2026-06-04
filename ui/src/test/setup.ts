// Vitest global setup (referenced by vite.config.ts -> test.setupFiles).
//
// Inter-test DOM cleanup for @testing-library/svelte v5 is guaranteed by the
// svelteTesting() plugin registered in vite.config.ts (per M3 reconciliation
// rule R4). Importing the library here makes its matchers/helpers available to
// component tests in later phases without relying on a bare import to
// auto-register cleanup.
import "@testing-library/svelte";

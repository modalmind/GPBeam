// Pure display helpers — the only "logic" allowed in the TS layer (M3 contract):
// byte sizes, progress percent, a human ETA label, and GiB<->bytes conversion
// for the Behavior tab's headroom slider. All total math comes pre-computed from
// the Rust AppState; these just render/convert it. No Tauri imports.
//
// This module is the single canonical union consumed by the popover
// (humanBytes/etaHuman/percent), the History tab (humanBytes) and the Behavior
// tab (bytesToGiB/giBToBytes). Authored once here in Phase 7.

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"] as const;

const GIB = 1024 * 1024 * 1024;

/** Human-readable IEC byte size; whole bytes under 1 KiB, one decimal above. */
export function humanBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  if (unit === 0) return `${Math.round(value)} B`;
  return `${value.toFixed(1)} ${UNITS[unit]}`;
}

/** Human ETA from a seconds count. null/undefined/negative -> em dash.
 *  Sub-hour: M:SS (zero-padded seconds). One hour and up: H:MM:SS. */
export function etaHuman(secs: number | null | undefined): string {
  if (secs === null || secs === undefined || !Number.isFinite(secs) || secs < 0) {
    return "—";
  }
  const total = Math.round(secs);
  const s = total % 60;
  const m = Math.floor(total / 60) % 60;
  const h = Math.floor(total / 3600);
  const ss = String(s).padStart(2, "0");
  if (h > 0) {
    const mm = String(m).padStart(2, "0");
    return `${h}:${mm}:${ss}`;
  }
  return `${m}:${ss}`;
}

/** Integer percent in 0..100; 0 when total <= 0 (no divide-by-zero). */
export function percent(done: number, total: number): number {
  if (!Number.isFinite(total) || total <= 0) return 0;
  const p = Math.round((done / total) * 100);
  return Math.max(0, Math.min(100, p));
}

/** Bytes -> GiB (1024^3). */
export function bytesToGiB(bytes: number): number {
  if (!Number.isFinite(bytes) || bytes <= 0) return 0;
  return bytes / GIB;
}

/** GiB -> bytes (1024^3). Negative GiB clamps to 0. */
export function giBToBytes(gib: number): number {
  if (!Number.isFinite(gib) || gib <= 0) return 0;
  return Math.round(gib * GIB);
}

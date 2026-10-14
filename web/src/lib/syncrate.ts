import { fmt } from "./format";

// What Home says about the speed of the first sync, worked out in the browser from the heights that arrive with
// every status poll (the host sends no rate). All of it is display only: `local_height` itself is never changed.

/** One poll: when it arrived (ms) and the validated height it carried. */
export type Sample = { t: number; h: number };

/** Samples older than this are dropped; the rate is the average over what is left. */
export const WINDOW_MS = 30_000;
/** The tip has not moved for this long: say what the node is probably waiting for. */
export const STALL_MS = 20_000;
/** A rate needs at least this much history, or two polls would pass for a speed. */
export const MIN_SPAN_MS = 5_000;

/** Adds a sample and drops the old ones. A height that goes down (restart, reorg, another network) starts afresh. */
export function pushSample(buf: Sample[], t: number, h: number): Sample[] {
  const last = buf[buf.length - 1];
  if (last && h < last.h) return [{ t, h }];
  const out = [...buf, { t, h }];
  // Keep one sample at or before the window edge so the span covers the whole window.
  let from = 0;
  while (from + 1 < out.length && out[from + 1].t <= t - WINDOW_MS) from++;
  return from ? out.slice(from) : out;
}

/** Blocks per second over the buffer, or null while there is too little history. */
export function rateOf(buf: Sample[]): number | null {
  if (buf.length < 2) return null;
  const a = buf[0];
  const b = buf[buf.length - 1];
  const span = b.t - a.t;
  if (span < MIN_SPAN_MS) return null;
  return Math.max(0, (b.h - a.h) / (span / 1000));
}

/** How long (ms) the height has been the same, as far as the buffer can tell (it holds about 30 s). */
export function flatFor(buf: Sample[]): number {
  const last = buf[buf.length - 1];
  if (!last) return 0;
  let first = last;
  for (let i = buf.length - 1; i >= 0 && buf[i].h === last.h; i--) first = buf[i];
  return last.t - first.t;
}

/** "about 42 blocks/s", "about 2.3 blocks/s"; null when the rate is unknown or rounds to nothing (then the stall hint applies). */
export function rateText(rate: number | null): string | null {
  if (rate === null || rate < 0.05) return null;
  return `about ${rate >= 10 ? fmt(Math.round(rate)) : rate.toFixed(1)} blocks/s`;
}

// Opt-in keystroke latency probe (frontend half). The backend decides whether it is on
// (`KEITORA_LATENCY_PROBE=1`); when off, `stamp()` returns undefined and nothing is
// sent or recorded.

import { latencyProbeEnabled } from "./ipc";

/** Samples per hop between console lines. */
export const BATCH = 200;

/** Nearest-rank percentile (`p` in 0..=100) of an ascending-sorted array. */
export function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const rank = Math.ceil((p / 100) * sorted.length);
  return sorted[Math.min(Math.max(rank, 1), sorted.length) - 1];
}

/** Collects per-hop samples; every `batch` samples of a hop it emits one summary line. */
export function createAggregator(
  log: (line: string) => void,
  batch = BATCH,
): (hop: string, ms: number) => void {
  const hops = new Map<string, number[]>();
  return (hop, ms) => {
    let buf = hops.get(hop);
    if (!buf) {
      buf = [];
      hops.set(hop, buf);
    }
    buf.push(ms);
    if (buf.length < batch) return;
    const sorted = [...buf].sort((a, b) => a - b);
    hops.set(hop, []);
    const f = (p: number) => percentile(sorted, p).toFixed(2);
    log(`latency-probe hop=${hop} n=${sorted.length} p50=${f(50)} p95=${f(95)} p99=${f(99)}`);
  };
}

let on = false;
const record = createAggregator((line) => console.info(line));
// Latest unanswered keystroke stamp per pane; the next output frame consumes it.
const pending = new Map<string, number>();

/** Reads the backend flag once; call at startup. Safe to call repeatedly. */
export function initLatencyProbe(): Promise<void> {
  return latencyProbeEnabled()
    .then((v) => {
      on = v;
    })
    .catch(() => {
      on = false;
    });
}

/** Epoch-ms timestamp for a keystroke, or undefined when the probe is off. */
export function stamp(): number | undefined {
  return on ? performance.timeOrigin + performance.now() : undefined;
}

/** Remember the stamp of the latest keystroke sent to `pane`. */
export function markPending(pane: string, t0: number | undefined): void {
  if (t0 !== undefined) pending.set(pane, t0);
}

/**
 * An output frame arrived for `pane`. When a keystroke is pending, records
 * `t0_to_frame_received` and returns a callback to run once `term.write` completes
 * (records `t0_to_write_done`); otherwise returns undefined.
 */
export function onFrame(pane: string): (() => void) | undefined {
  const t0 = pending.get(pane);
  if (t0 === undefined) return undefined;
  pending.delete(pane);
  record("t0_to_frame_received", Math.max(0, performance.timeOrigin + performance.now() - t0));
  return () =>
    record("t0_to_write_done", Math.max(0, performance.timeOrigin + performance.now() - t0));
}

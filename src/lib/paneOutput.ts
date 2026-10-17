// The receiving end of one attach's output channel. Frames can arrive while the
// attach call is still in flight, before the attach scrollback has been written;
// the gate holds them until `open()` so they land after it, in order.

import { decodeBase64 } from "./base64";
import type { OutputMsg } from "./ipc";

/**
 * Where an open gate delivers: decoded frame bytes, and the session's exit. `full`
 * marks a whole-viewport repaint (herdr's frame after a resize), which supersedes any output of the stream still waiting to be written.
 */
export interface PaneOutputTarget {
  write(bytes: Uint8Array, full: boolean): void;
  exit(): void;
}

export interface OutputGate {
  /** The channel handler for this attach. */
  readonly onMessage: (msg: OutputMsg) => void;
  /** The scrollback is written: flush what arrived meanwhile, then pass through. */
  open(): void;
  /** This attach is over (failed, replaced or torn down): drop everything. */
  close(): void;
}

export function createOutputGate(target: PaneOutputTarget): OutputGate {
  let state: "buffering" | "open" | "closed" = "buffering";
  let pending: OutputMsg[] = [];

  const deliver = (msg: OutputMsg) => {
    if (msg.type === "frame") target.write(decodeBase64(msg.data), msg.full);
    else target.exit();
  };

  return {
    onMessage(msg) {
      if (state === "closed") return;
      if (state === "buffering") pending.push(msg);
      else deliver(msg);
    },
    open() {
      if (state !== "buffering") return;
      state = "open";
      const queued = pending;
      pending = [];
      for (const msg of queued) deliver(msg);
    },
    close() {
      state = "closed";
      pending = [];
    },
  };
}

// Per-pane output coalescer. Frames pushed while the pane is visible are joined
// into one `write` per animation frame. While the pane is hidden they wait in a
// bounded buffer instead; a pane that outran the bound drops it and asks for a
// repaint when it is shown again, but only when its backend can repaint. One that
// cannot writes the buffer out instead, so it never loses output.
//
// A visible pane is bounded too: animation frames stop while the window is
// minimized or occluded, so a frame not run within `fallbackMs` is replaced by a
// timer, and a buffer past `limit` is written at once.
//
// A `full` chunk repaints the whole viewport, so it supersedes everything still
// queued before it.

/** Most bytes a pane keeps queued before it writes them out (or, hidden, drops them). */
export const HIDDEN_BUFFER_LIMIT = 256 * 1024;

/** How long a visible pane waits for an animation frame before a timer flushes instead. */
export const FRAME_FALLBACK_MS = 50;

export interface FrameScheduler {
  request(cb: () => void): number;
  cancel(handle: number): void;
  /** A one-shot timer (the fallback for animation frames that never come). */
  timeout(cb: () => void, ms: number): number;
  clearTimeout(handle: number): void;
}

export interface FrameCoalescerOptions {
  /** Delivers one joined chunk; `done` runs once it has been processed. */
  write(bytes: Uint8Array, done: () => void): void;
  /** The hidden buffer overflowed and was dropped: the pane must be repainted on show. */
  onNeedsRepaint(): void;
  visible?: boolean;
  /** Whether the backend can repaint (see `setRepaintable`); false until told. */
  repaintable?: boolean;
  limit?: number;
  fallbackMs?: number;
  scheduler?: FrameScheduler;
}

export interface FrameCoalescer {
  /**
   * Queue output; `written` runs when the chunk containing it has been written.
   * A `full` chunk replaces whatever is still queued.
   */
  push(bytes: Uint8Array, written?: () => void, full?: boolean): void;
  setVisible(visible: boolean): void;
  /** Whether the backend repaints on request; one that cannot never has output dropped. */
  setRepaintable(repaintable: boolean): void;
  /**
   * The session ended: write out everything still queued (hidden or not), then run
   * `then` once it has been processed. A repaint owed is forgotten (nothing to repaint).
   * A `reset` or `dispose` before then cancels `then` (a new attach or session took over).
   */
  end(then: () => void): void;
  /** Drop everything queued (a new attach supersedes it) and cancel a pending `end`. */
  reset(): void;
  /** Drop everything queued and cancel the pending frame. */
  dispose(): void;
}

const browserScheduler: FrameScheduler = {
  request: (cb) => requestAnimationFrame(cb),
  cancel: (handle) => cancelAnimationFrame(handle),
  timeout: (cb, ms) => window.setTimeout(cb, ms),
  clearTimeout: (handle) => window.clearTimeout(handle),
};

function join(chunks: Uint8Array[], size: number): Uint8Array {
  if (chunks.length === 1) return chunks[0];
  const out = new Uint8Array(size);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}

export function createFrameCoalescer(opts: FrameCoalescerOptions): FrameCoalescer {
  const limit = opts.limit ?? HIDDEN_BUFFER_LIMIT;
  const fallbackMs = opts.fallbackMs ?? FRAME_FALLBACK_MS;
  const scheduler = opts.scheduler ?? browserScheduler;
  let visible = opts.visible ?? true;
  let repaintable = opts.repaintable ?? false;
  let chunks: Uint8Array[] = [];
  let callbacks: (() => void)[] = [];
  let size = 0;
  let needsRepaint = false;
  let frame: number | null = null;
  let timer: number | null = null;
  // Bumped by reset/dispose, so an `end` still waiting on its write is cancelled.
  let epoch = 0;

  const clear = () => {
    chunks = [];
    callbacks = [];
    size = 0;
  };

  const cancelFrame = () => {
    if (frame != null) {
      scheduler.cancel(frame);
      frame = null;
    }
    if (timer != null) {
      scheduler.clearTimeout(timer);
      timer = null;
    }
  };

  /** Write everything queued as one chunk; `then` runs once it is processed. */
  const flush = (then?: () => void) => {
    cancelFrame();
    if (chunks.length === 0) {
      then?.();
      return;
    }
    const bytes = join(chunks, size);
    const done = callbacks;
    clear();
    opts.write(bytes, () => {
      for (const cb of done) cb();
      then?.();
    });
  };

  const schedule = () => {
    if (chunks.length === 0) return;
    if (frame == null) frame = scheduler.request(() => flush());
    if (timer == null) timer = scheduler.timeout(() => flush(), fallbackMs);
  };

  return {
    push(bytes, written, full = false) {
      if (full) {
        // The whole viewport again: nothing queued before it still matters, and a
        // repaint owed for a dropped buffer is paid by this frame. The written
        // callbacks stay, hidden or not (they belong to the output this shows).
        chunks = [];
        size = 0;
        needsRepaint = false;
      }
      if (!visible) {
        // A repaint is already owed, which makes anything buffered since redundant.
        if (needsRepaint) return;
        if (size + bytes.length > limit) {
          if (repaintable) {
            clear();
            needsRepaint = true;
            return;
          }
          // No repaint to fall back on: write what is buffered (hidden) instead.
          flush();
        }
      }
      chunks.push(bytes);
      size += bytes.length;
      if (written) callbacks.push(written);
      if (!visible) return;
      // Frames may be paused (minimized / occluded window): never let it grow unbounded.
      if (size > limit) flush();
      else schedule();
    },
    setVisible(next) {
      if (next === visible) return;
      visible = next;
      if (!next) {
        cancelFrame();
        return;
      }
      if (needsRepaint) {
        needsRepaint = false;
        clear();
        opts.onNeedsRepaint();
        return;
      }
      schedule();
    },
    setRepaintable(next) {
      repaintable = next;
    },
    end(then) {
      needsRepaint = false;
      const at = epoch;
      flush(() => {
        if (at === epoch) then();
      });
    },
    reset() {
      epoch++;
      cancelFrame();
      clear();
      needsRepaint = false;
    },
    dispose() {
      epoch++;
      cancelFrame();
      clear();
    },
  };
}

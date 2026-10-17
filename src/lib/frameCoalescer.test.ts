import { describe, expect, it } from "vitest";
import { createFrameCoalescer, type FrameCoalescerOptions } from "./frameCoalescer";

const bytes = (s: string) => new TextEncoder().encode(s);
const text = (b: Uint8Array) => new TextDecoder().decode(b);

function harness(opts: Partial<FrameCoalescerOptions> = {}) {
  const writes: string[] = [];
  const dones: (() => void)[] = [];
  let repaints = 0;
  let next = 1;
  const frames = new Map<number, () => void>();
  const timers = new Map<number, () => void>();
  const coalescer = createFrameCoalescer({
    write: (b, done) => {
      writes.push(text(b));
      dones.push(done);
    },
    onNeedsRepaint: () => {
      repaints++;
    },
    scheduler: {
      request: (cb) => {
        frames.set(next, cb);
        return next++;
      },
      cancel: (h) => {
        frames.delete(h);
      },
      timeout: (cb) => {
        timers.set(next, cb);
        return next++;
      },
      clearTimeout: (h) => {
        timers.delete(h);
      },
    },
    ...opts,
  });
  const runFrame = () => {
    const pending = [...frames.values()];
    frames.clear();
    for (const cb of pending) cb();
  };
  const runTimers = () => {
    const pending = [...timers.values()];
    timers.clear();
    for (const cb of pending) cb();
  };
  return {
    coalescer,
    writes,
    dones,
    runFrame,
    runTimers,
    frames,
    timers,
    repaints: () => repaints,
  };
}

describe("createFrameCoalescer", () => {
  it("writes once per frame and keeps order", () => {
    const h = harness();
    h.coalescer.push(bytes("a"));
    h.coalescer.push(bytes("b"));
    h.coalescer.push(bytes("c"));
    expect(h.writes).toEqual([]);
    expect(h.frames.size).toBe(1);
    h.runFrame();
    expect(h.writes).toEqual(["abc"]);
    h.coalescer.push(bytes("d"));
    h.runFrame();
    expect(h.writes).toEqual(["abc", "d"]);
  });

  it("runs every written callback after the joined write completes", () => {
    const h = harness();
    const seen: string[] = [];
    h.coalescer.push(bytes("a"), () => seen.push("a"));
    h.coalescer.push(bytes("b"), () => seen.push("b"));
    h.runFrame();
    expect(seen).toEqual([]);
    h.dones[0]();
    expect(seen).toEqual(["a", "b"]);
  });

  it("buffers while hidden and flushes on show", () => {
    const h = harness({ visible: false });
    h.coalescer.push(bytes("a"));
    h.coalescer.push(bytes("b"));
    expect(h.frames.size).toBe(0);
    h.coalescer.setVisible(true);
    h.runFrame();
    expect(h.writes).toEqual(["ab"]);
    expect(h.repaints()).toBe(0);
  });

  it("holds an already scheduled frame back when hidden", () => {
    const h = harness();
    h.coalescer.push(bytes("a"));
    h.coalescer.setVisible(false);
    expect(h.frames.size).toBe(0);
    h.coalescer.push(bytes("b"));
    h.coalescer.setVisible(true);
    h.runFrame();
    expect(h.writes).toEqual(["ab"]);
  });

  it("drops the buffer and asks for a repaint on overflow", () => {
    const h = harness({ visible: false, limit: 4, repaintable: true });
    h.coalescer.push(bytes("abc"));
    h.coalescer.push(bytes("de"));
    h.coalescer.push(bytes("f"));
    h.coalescer.setVisible(true);
    h.runFrame();
    expect(h.writes).toEqual([]);
    expect(h.repaints()).toBe(1);
    h.coalescer.push(bytes("g"));
    h.runFrame();
    expect(h.writes).toEqual(["g"]);
  });

  it("never drops output on a backend that cannot repaint", () => {
    const h = harness({ visible: false, limit: 4 });
    h.coalescer.push(bytes("abc"));
    h.coalescer.push(bytes("de")); // over the bound: "abc" is written while hidden
    expect(h.writes).toEqual(["abc"]);
    h.coalescer.push(bytes("f"));
    h.coalescer.setVisible(true);
    h.runFrame();
    expect(h.writes).toEqual(["abc", "def"]);
    expect(h.repaints()).toBe(0);
  });

  it("learns at attach whether the backend repaints", () => {
    const h = harness({ visible: false, limit: 4 });
    h.coalescer.setRepaintable(true);
    h.coalescer.push(bytes("abcde"));
    h.coalescer.setVisible(true);
    expect(h.repaints()).toBe(1);
    expect(h.writes).toEqual([]);
  });

  it("flushes on a timer when animation frames stop coming", () => {
    const h = harness();
    h.coalescer.push(bytes("a"));
    h.coalescer.push(bytes("b"));
    // Minimized: the frame never runs; the fallback timer writes instead.
    h.runTimers();
    expect(h.writes).toEqual(["ab"]);
    expect(h.frames.size).toBe(0);
    // And the frame cancels the timer when it does come.
    h.coalescer.push(bytes("c"));
    h.runFrame();
    expect(h.writes).toEqual(["ab", "c"]);
    expect(h.timers.size).toBe(0);
  });

  it("writes a visible buffer past the bound at once", () => {
    const h = harness({ limit: 4 });
    const seen: string[] = [];
    h.coalescer.push(bytes("abc"), () => seen.push("abc"));
    h.coalescer.push(bytes("de"), () => seen.push("de"));
    expect(h.writes).toEqual(["abcde"]);
    expect(h.frames.size).toBe(0);
    expect(h.timers.size).toBe(0);
    h.dones[0]();
    expect(seen).toEqual(["abc", "de"]);
  });

  it("lets a full frame replace what a visible pane still queues", () => {
    const h = harness();
    const seen: string[] = [];
    h.coalescer.push(bytes("old"), () => seen.push("old"));
    h.coalescer.push(bytes("FULL"), () => seen.push("full"), true);
    h.coalescer.push(bytes("+"));
    h.runFrame();
    expect(h.writes).toEqual(["FULL+"]);
    h.dones[0]();
    expect(seen).toEqual(["old", "full"]);
  });

  it("lets a full frame replace a hidden buffer and settle an owed repaint", () => {
    const h = harness({ visible: false, limit: 4, repaintable: true });
    h.coalescer.push(bytes("abcde")); // overflow: a repaint is owed
    h.coalescer.push(bytes("x")); // redundant while owed
    h.coalescer.push(bytes("FUL"), undefined, true);
    h.coalescer.push(bytes("+"));
    h.coalescer.setVisible(true);
    h.runFrame();
    expect(h.writes).toEqual(["FUL+"]);
    expect(h.repaints()).toBe(0);
  });

  it("ends by writing what is queued, then reporting the exit", () => {
    const h = harness();
    const log: string[] = [];
    h.coalescer.push(bytes("bye"));
    h.coalescer.end(() => log.push("ended"));
    expect(h.writes).toEqual(["bye"]);
    expect(log).toEqual([]);
    h.dones[0]();
    expect(log).toEqual(["ended"]);
    expect(h.frames.size).toBe(0);
    expect(h.timers.size).toBe(0);
  });

  it("ends a hidden pane the same way, forgetting a repaint", () => {
    const h = harness({ visible: false, limit: 4, repaintable: true });
    h.coalescer.push(bytes("abcde")); // dropped: a repaint is owed
    const log: string[] = [];
    h.coalescer.end(() => log.push("ended"));
    expect(log).toEqual(["ended"]);
    h.coalescer.setVisible(true);
    expect(h.repaints()).toBe(0);

    const k = harness({ visible: false });
    k.coalescer.push(bytes("tail"));
    k.coalescer.end(() => log.push("ended too"));
    expect(k.writes).toEqual(["tail"]);
    k.dones[0]();
    expect(log).toEqual(["ended", "ended too"]);
  });

  it("keeps the written callbacks of a hidden pane a full frame supersedes", () => {
    const h = harness({ visible: false });
    const seen: string[] = [];
    h.coalescer.push(bytes("old"), () => seen.push("old"));
    h.coalescer.push(bytes("FULL"), () => seen.push("full"), true);
    h.coalescer.setVisible(true);
    h.runFrame();
    expect(h.writes).toEqual(["FULL"]);
    h.dones[0]();
    expect(seen).toEqual(["old", "full"]);
  });

  it("a reset cancels an end still waiting on its write", () => {
    const h = harness();
    const log: string[] = [];
    h.coalescer.push(bytes("bye"));
    h.coalescer.end(() => log.push("ended"));
    // A new attach (or a restart) takes over before the write is processed.
    h.coalescer.reset();
    h.dones[0]();
    expect(log).toEqual([]);

    const k = harness();
    k.coalescer.push(bytes("bye"));
    k.coalescer.end(() => log.push("disposed"));
    k.coalescer.dispose();
    k.dones[0]();
    expect(log).toEqual([]);
  });

  it("reset drops queued output and the pending frame", () => {
    const h = harness();
    h.coalescer.push(bytes("a"));
    h.coalescer.reset();
    expect(h.frames.size).toBe(0);
    h.runFrame();
    expect(h.writes).toEqual([]);
  });

  it("dispose cancels the pending frame", () => {
    const h = harness();
    h.coalescer.push(bytes("a"));
    h.coalescer.dispose();
    expect(h.frames.size).toBe(0);
    expect(h.timers.size).toBe(0);
  });
});

import { describe, expect, it } from "vitest";
import { createOutputGate } from "./paneOutput";

/** A target that records what the pane would have shown, as text. */
function recorder() {
  const log: string[] = [];
  const fulls: boolean[] = [];
  return {
    log,
    fulls,
    target: {
      write: (bytes: Uint8Array, full: boolean) => {
        log.push(new TextDecoder().decode(bytes));
        fulls.push(full);
      },
      exit: () => log.push("<exit>"),
    },
  };
}

const frame = (text: string, full = false) => ({ type: "frame", data: btoa(text), full }) as const;

describe("createOutputGate", () => {
  it("holds frames until the scrollback is written, then flushes them in order", () => {
    const { log, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("one"));
    gate.onMessage(frame("two", true));
    expect(log).toEqual([]);

    log.push("<scrollback>");
    gate.open();
    expect(log).toEqual(["<scrollback>", "one", "two"]);

    gate.onMessage(frame("three"));
    expect(log).toEqual(["<scrollback>", "one", "two", "three"]);
  });

  it("delivers an exit in its place in the stream", () => {
    const { log, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("bye"));
    gate.onMessage({ type: "exit" });
    gate.open();
    expect(log).toEqual(["bye", "<exit>"]);
  });

  it("drops everything once closed, buffered or not", () => {
    const { log, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("stale"));
    gate.close();
    gate.open();
    gate.onMessage(frame("late"));
    gate.onMessage({ type: "exit" });
    expect(log).toEqual([]);
  });

  it("passes each frame's full flag on", () => {
    const { log, fulls, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("diff"));
    gate.open();
    gate.onMessage(frame("repaint", true));
    expect(log).toEqual(["diff", "repaint"]);
    expect(fulls).toEqual([false, true]);
  });

  it("opens only once", () => {
    const { log, target } = recorder();
    const gate = createOutputGate(target);
    gate.onMessage(frame("a"));
    gate.open();
    gate.open();
    expect(log).toEqual(["a"]);
  });
});

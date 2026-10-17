import { describe, expect, it } from "vitest";
import {
  applyOrder,
  latestWins,
  moveToSlot,
  sameKeys,
  sameOrder,
  settleOverride,
} from "./railOrder";

const keys = ["a", "b", "c", "d"];

describe("settleOverride", () => {
  it("is null with no override", () => {
    expect(settleOverride(null, keys)).toBeNull();
  });

  it("clears once the incoming order matches the override", () => {
    expect(settleOverride(["b", "a", "c", "d"], ["b", "a", "c", "d"])).toBeNull();
  });

  it("keeps the override while the registry still has the old order", () => {
    const override = ["b", "a", "c", "d"];
    expect(settleOverride(override, keys)).toBe(override);
  });

  it("drops the override when a project was added or removed", () => {
    expect(settleOverride(["b", "a", "c", "d"], ["a", "b", "c"])).toBeNull();
    expect(settleOverride(["b", "a", "c", "d"], ["a", "b", "c", "d", "e"])).toBeNull();
    expect(settleOverride(["b", "a", "c", "d"], ["a", "b", "c", "e"])).toBeNull();
  });
});

describe("applyOrder", () => {
  const items = keys.map((k) => ({ k }));
  const keyOf = (it: { k: string }) => it.k;

  it("returns the items as they are with no order", () => {
    expect(applyOrder(items, null, keyOf).map(keyOf)).toEqual(keys);
  });

  it("follows the order", () => {
    expect(applyOrder(items, ["d", "c", "b", "a"], keyOf).map(keyOf)).toEqual(["d", "c", "b", "a"]);
  });

  it("appends items the order does not name and skips keys with no item", () => {
    expect(applyOrder(items, ["c", "x", "a"], keyOf).map(keyOf)).toEqual(["c", "a", "b", "d"]);
  });
});

describe("sameOrder", () => {
  it("is true only for the same keys in the same order", () => {
    expect(sameOrder(["a", "b"], ["a", "b"])).toBe(true);
    expect(sameOrder(["a", "b"], ["b", "a"])).toBe(false);
    expect(sameOrder(["a"], ["a", "b"])).toBe(false);
  });
});

describe("sameKeys", () => {
  it("compares key sets in any order, across arrays and sets", () => {
    expect(sameKeys(["a", "b"], ["b", "a"])).toBe(true);
    expect(sameKeys(new Set(["a", "b"]), new Set(["b", "a"]))).toBe(true);
    expect(sameKeys(["a", "b"], ["a", "c"])).toBe(false);
    expect(sameKeys(["a"], new Set(["a", "b"]))).toBe(false);
  });
});

describe("latestWins", () => {
  // A persist whose calls stay pending until the test settles them, in order.
  function controlled() {
    const calls: { arg: string; resolve: () => void; reject: (e: unknown) => void }[] = [];
    const persist = (arg: string) =>
      new Promise<void>((resolve, reject) => {
        calls.push({ arg, resolve, reject });
      });
    return { calls, persist };
  }
  const flush = async () => {
    for (let i = 0; i < 5; i++) await Promise.resolve();
  };

  it("runs one call at a time and the latest waiting call replaces earlier ones", async () => {
    const { calls, persist } = controlled();
    const run = latestWins(persist);
    const outcomes: string[] = [];
    const track = (name: string, p: Promise<void>) =>
      p.then(
        () => outcomes.push(`${name} ok`),
        () => outcomes.push(`${name} err`),
      );

    track("a", run("a"));
    track("b", run("b"));
    track("c", run("c"));
    await flush();
    expect(calls.map((c) => c.arg)).toEqual(["a"]);
    // "b" was replaced by "c" before it ran: it settles without running.
    expect(outcomes).toEqual(["b ok"]);

    calls[0].resolve();
    await flush();
    expect(calls.map((c) => c.arg)).toEqual(["a", "c"]);
    calls[1].resolve();
    await flush();
    expect(outcomes).toEqual(["b ok", "a ok", "c ok"]);
  });

  it("reports a failed run to its own caller and keeps going", async () => {
    const { calls, persist } = controlled();
    const run = latestWins(persist);
    const a = run("a");
    const b = run("b");
    calls[0].reject(new Error("boom"));
    await expect(a).rejects.toThrow("boom");
    await flush();
    expect(calls.map((c) => c.arg)).toEqual(["a", "b"]);
    calls[1].resolve();
    await expect(b).resolves.toBeUndefined();
  });

  it("runs straight away again once idle", async () => {
    const { calls, persist } = controlled();
    const run = latestWins(persist);
    const a = run("a");
    calls[0].resolve();
    await a;
    await flush();
    void run("b");
    expect(calls.map((c) => c.arg)).toEqual(["a", "b"]);
  });
});

describe("moveToSlot", () => {
  it("moves a key down into a later gap", () => {
    expect(moveToSlot(keys, "a", 2)).toEqual(["b", "a", "c", "d"]);
    expect(moveToSlot(keys, "a", 4)).toEqual(["b", "c", "d", "a"]);
  });

  it("moves a key up into an earlier gap", () => {
    expect(moveToSlot(keys, "d", 0)).toEqual(["d", "a", "b", "c"]);
    expect(moveToSlot(keys, "c", 1)).toEqual(["a", "c", "b", "d"]);
  });

  it("is null for the gaps right around the key itself", () => {
    expect(moveToSlot(keys, "b", 1)).toBeNull();
    expect(moveToSlot(keys, "b", 2)).toBeNull();
  });

  it("is null for an unknown key or an out-of-range slot", () => {
    expect(moveToSlot(keys, "z", 0)).toBeNull();
    expect(moveToSlot(keys, "a", -1)).toBeNull();
    expect(moveToSlot(keys, "a", 5)).toBeNull();
  });
});

import { describe, expect, it } from "vitest";
import {
  applyExpanded,
  collapseOthers,
  ensureExpanded,
  expandAll,
  forget,
  isStoredExpanded,
  toggle,
} from "./railExpanded";

const sorted = (s: ReadonlySet<string>) => [...s].sort();

describe("toggle", () => {
  it("opens a closed group and closes an open one", () => {
    const s = new Set(["a"]);
    expect(sorted(toggle(s, "b"))).toEqual(["a", "b"]);
    expect(sorted(toggle(s, "a"))).toEqual([]);
  });

  it("does not mutate its input", () => {
    const s = new Set(["a"]);
    toggle(s, "a");
    toggle(s, "b");
    expect(sorted(s)).toEqual(["a"]);
  });
});

describe("expandAll", () => {
  it("opens every given group and keeps the ones already open", () => {
    const s = new Set(["x"]);
    expect(sorted(expandAll(s, ["a", "b"]))).toEqual(["a", "b", "x"]);
    expect(sorted(s)).toEqual(["x"]);
  });
});

describe("collapseOthers", () => {
  it("closes every group but the given one", () => {
    expect(sorted(collapseOthers("b"))).toEqual(["b"]);
  });
});

describe("ensureExpanded", () => {
  it("opens a closed group without touching the others or the input", () => {
    const s = new Set(["a"]);
    expect(sorted(ensureExpanded(s, "b"))).toEqual(["a", "b"]);
    expect(sorted(s)).toEqual(["a"]);
  });

  it("returns the same set when the group is already open", () => {
    const s = new Set(["a"]);
    expect(ensureExpanded(s, "a")).toBe(s);
  });
});

describe("forget", () => {
  it("drops the removed project's key without touching the input", () => {
    const s = new Set(["a", "gone", "b"]);
    expect(sorted(forget(s, "gone"))).toEqual(["a", "b"]);
    expect(sorted(s)).toEqual(["a", "b", "gone"]);
  });

  it("returns the same set when the key was not open", () => {
    const s = new Set(["a"]);
    expect(forget(s, "b")).toBe(s);
  });
});

describe("isStoredExpanded", () => {
  it("accepts null and string arrays", () => {
    expect(isStoredExpanded(null)).toBe(true);
    expect(isStoredExpanded([])).toBe(true);
    expect(isStoredExpanded(["a", "b"])).toBe(true);
  });

  it("rejects anything else", () => {
    expect(isStoredExpanded(undefined)).toBe(false);
    expect(isStoredExpanded("a")).toBe(false);
    expect(isStoredExpanded(["a", 1])).toBe(false);
    expect(isStoredExpanded({ a: true })).toBe(false);
  });
});

describe("applyExpanded", () => {
  it("returns the same array when the set is unchanged", () => {
    const prev = ["a", "b"];
    expect(applyExpanded(prev, (s) => ensureExpanded(s, "a"))).toBe(prev);
    expect(applyExpanded(prev, (s) => forget(s, "c"))).toBe(prev);
  });

  it("returns a new array when the set changes", () => {
    expect(applyExpanded(["a"], (s) => toggle(s, "b"))?.sort()).toEqual(["a", "b"]);
    expect(applyExpanded(["a", "gone"], (s) => forget(s, "gone"))).toEqual(["a"]);
  });

  it("keeps the never-stored null when nothing ends up open", () => {
    expect(applyExpanded(null, (s) => forget(s, "a"))).toBeNull();
  });

  it("stores from null once something opens", () => {
    expect(applyExpanded(null, (s) => ensureExpanded(s, "a"))).toEqual(["a"]);
  });

  it("stores an empty list when the last open group closes", () => {
    expect(applyExpanded(["a"], (s) => toggle(s, "a"))).toEqual([]);
  });
});

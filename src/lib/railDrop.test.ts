import { describe, expect, it } from "vitest";
import {
  insertionIndex,
  isNoopSlot,
  landingIndex,
  lineOffset,
  slotTargetY,
  stepSlot,
  type VSpan,
} from "./railDrop";

// Three groups: a collapsed one, an open one (tall card, header on top), a collapsed one.
const heads: VSpan[] = [
  { top: 0, bottom: 44 },
  { top: 50, bottom: 94 },
  { top: 300, bottom: 344 },
];
const cards: VSpan[] = [
  { top: 0, bottom: 44 },
  { top: 50, bottom: 294 },
  { top: 300, bottom: 344 },
];

describe("insertionIndex", () => {
  it("lands before a header whose midpoint is below the chip", () => {
    expect(insertionIndex(heads, -10)).toBe(0);
    expect(insertionIndex(heads, 21)).toBe(0);
    expect(insertionIndex(heads, 23)).toBe(1);
  });

  it("lands after an open group once the chip clears its header", () => {
    // Over the open group's lanes (well below its header) is already "after it".
    expect(insertionIndex(heads, 80)).toBe(2);
    expect(insertionIndex(heads, 250)).toBe(2);
  });

  it("lands at the end below the last header's midpoint", () => {
    expect(insertionIndex(heads, 330)).toBe(3);
    expect(insertionIndex(heads, 9999)).toBe(3);
  });

  it("is 0 for an empty list", () => {
    expect(insertionIndex([], 10)).toBe(0);
  });
});

describe("isNoopSlot", () => {
  it("is the slot just before and just after the group", () => {
    expect(isNoopSlot(1, 1)).toBe(true);
    expect(isNoopSlot(2, 1)).toBe(true);
    expect(isNoopSlot(0, 1)).toBe(false);
    expect(isNoopSlot(3, 1)).toBe(false);
  });
});

describe("landingIndex", () => {
  it("is the slot moving up, one less moving down, and in place for a no-op slot", () => {
    expect(landingIndex(0, 2)).toBe(0);
    expect(landingIndex(4, 2)).toBe(3);
    expect(landingIndex(2, 2)).toBe(2);
    expect(landingIndex(3, 2)).toBe(2);
  });
});

describe("lineOffset", () => {
  it("centres the line in the gap between cards, relative to the list", () => {
    expect(lineOffset(cards, 1, 0)).toBe(47);
    expect(lineOffset(cards, 2, 0)).toBe(297);
    expect(lineOffset(cards, 2, 100)).toBe(197);
  });

  it("sits just outside the first and last card", () => {
    expect(lineOffset(cards, 0, 0)).toBe(-3);
    expect(lineOffset(cards, 3, 0)).toBe(347);
  });

  it("clamps out-of-range slots and is null with no cards", () => {
    expect(lineOffset(cards, -4, 0)).toBe(-3);
    expect(lineOffset(cards, 9, 0)).toBe(347);
    expect(lineOffset([], 0, 0)).toBeNull();
  });
});

describe("stepSlot", () => {
  it("skips the two no-op slots around the group", () => {
    // Group 1 of 3; slots 1 and 2 leave it in place.
    expect(stepSlot(2, 1, 1, 3)).toBe(3);
    expect(stepSlot(2, -1, 1, 3)).toBe(0);
    expect(stepSlot(1, 1, 1, 3)).toBe(3);
  });

  it("steps one slot at a time away from the group", () => {
    expect(stepSlot(0, 1, 3, 5)).toBe(1);
    expect(stepSlot(1, 1, 3, 5)).toBe(2);
    expect(stepSlot(2, 1, 3, 5)).toBe(5);
  });

  it("stays put at either end", () => {
    expect(stepSlot(0, -1, 2, 3)).toBe(0);
    expect(stepSlot(3, 1, 0, 3)).toBe(3);
    // The last group can only go up.
    expect(stepSlot(3, 1, 2, 3)).toBe(3);
  });

  it("goes nowhere with a single group", () => {
    expect(stepSlot(1, 1, 0, 1)).toBe(1);
    expect(stepSlot(1, -1, 0, 1)).toBe(1);
  });
});

describe("slotTargetY", () => {
  it("maps back to the same slot through insertionIndex", () => {
    for (let s = 0; s <= heads.length; s++) {
      const y = slotTargetY(heads, s);
      expect(y).not.toBeNull();
      expect(insertionIndex(heads, y as number)).toBe(s);
    }
  });

  it("is null with no headers", () => {
    expect(slotTargetY([], 0)).toBeNull();
  });
});

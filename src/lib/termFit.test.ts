import { describe, expect, it } from "vitest";
import {
  accumulateWheelLines,
  clampPtySize,
  dimsAreSane,
  dimsEqual,
  FALLBACK_ROW_PX,
  isAtBottom,
  MIN_PTY,
} from "./termFit";

// Unit tests for the pure terminal size/fit decision helpers.

describe("clampPtySize", () => {
  it("floors rows below the minimum", () => {
    expect(clampPtySize({ rows: 1, cols: 80 })).toEqual({ rows: MIN_PTY.rows, cols: 80 });
  });

  it("floors cols below the minimum", () => {
    expect(clampPtySize({ rows: 24, cols: 3 })).toEqual({ rows: 24, cols: MIN_PTY.cols });
  });

  it("floors both axes together", () => {
    expect(clampPtySize({ rows: 0, cols: 0 })).toEqual({
      rows: MIN_PTY.rows,
      cols: MIN_PTY.cols,
    });
  });

  it("passes through values above the floor unchanged", () => {
    expect(clampPtySize({ rows: 40, cols: 120 })).toEqual({ rows: 40, cols: 120 });
  });
});

describe("dimsAreSane", () => {
  it("rejects undefined and null", () => {
    expect(dimsAreSane(undefined)).toBe(false);
    expect(dimsAreSane(null)).toBe(false);
  });

  it("rejects a zero axis", () => {
    expect(dimsAreSane({ rows: 0, cols: 80 })).toBe(false);
    expect(dimsAreSane({ rows: 24, cols: 0 })).toBe(false);
  });

  it("rejects negatives", () => {
    expect(dimsAreSane({ rows: -1, cols: 80 })).toBe(false);
  });

  it("rejects non-finite measurements", () => {
    expect(dimsAreSane({ rows: NaN, cols: 80 })).toBe(false);
    expect(dimsAreSane({ rows: 24, cols: Infinity })).toBe(false);
  });

  it("accepts a positive, finite proposal", () => {
    expect(dimsAreSane({ rows: 24, cols: 80 })).toBe(true);
  });
});

describe("dimsEqual", () => {
  it("is true for identical geometry", () => {
    expect(dimsEqual({ rows: 24, cols: 80 }, { rows: 24, cols: 80 })).toBe(true);
  });

  it("is false when rows differ", () => {
    expect(dimsEqual({ rows: 24, cols: 80 }, { rows: 25, cols: 80 })).toBe(false);
  });

  it("is false when cols differ", () => {
    expect(dimsEqual({ rows: 24, cols: 80 }, { rows: 24, cols: 81 })).toBe(false);
  });
});

describe("isAtBottom", () => {
  it("is true when the viewport is pinned to the tail", () => {
    expect(isAtBottom(100, 100)).toBe(true);
  });

  it("is false when scrolled up", () => {
    expect(isAtBottom(40, 100)).toBe(false);
  });

  it("is true when over-scrolled past the tail", () => {
    expect(isAtBottom(105, 100)).toBe(true);
  });
});

describe("accumulateWheelLines", () => {
  it("accumulates a sub-line delta without emitting a line", () => {
    const { lines, remainder } = accumulateWheelLines(10, 17, 0);
    expect(lines).toBe(0);
    expect(remainder).toBeCloseTo(10 / 17, 5);
  });

  it("carries the remainder across calls to cross a line boundary", () => {
    const first = accumulateWheelLines(10, 17, 0);
    const second = accumulateWheelLines(10, 17, first.remainder);
    expect(second.lines).toBe(1);
    expect(second.remainder).toBeCloseTo(20 / 17 - 1, 5);
  });

  it("emits exactly one line for one row of pixels with no remainder", () => {
    expect(accumulateWheelLines(17, 17, 0)).toEqual({ lines: 1, remainder: 0 });
  });

  it("emits multiple lines for a large delta", () => {
    expect(accumulateWheelLines(170, 17, 0).lines).toBe(10);
  });

  it("preserves sign for upward scrolling", () => {
    expect(accumulateWheelLines(-17, 17, 0)).toEqual({ lines: -1, remainder: 0 });
    const { lines, remainder } = accumulateWheelLines(-10, 17, 0);
    expect(lines).toBe(0);
    expect(remainder).toBeCloseTo(-10 / 17, 5);
  });

  it("never returns NaN/Infinity for a zero or non-finite row height", () => {
    for (const badRow of [0, -5, NaN, Infinity, -Infinity]) {
      const { lines, remainder } = accumulateWheelLines(FALLBACK_ROW_PX, badRow, 0);
      expect(Number.isFinite(lines)).toBe(true);
      expect(Number.isFinite(remainder)).toBe(true);
      expect(lines).toBe(1);
    }
  });

  it("treats a non-finite delta as no movement", () => {
    expect(accumulateWheelLines(NaN, 17, 0)).toEqual({ lines: 0, remainder: 0 });
    expect(accumulateWheelLines(Infinity, 17, 0)).toEqual({ lines: 0, remainder: 0 });
  });

  it("treats a non-finite prior remainder as zero", () => {
    expect(accumulateWheelLines(17, 17, NaN).lines).toBe(1);
  });

  it("drops an opposite-sign remainder on direction reversal", () => {
    const { lines, remainder } = accumulateWheelLines(-5, 17, 0.9);
    expect(lines).toBe(0);
    expect(remainder).toBeLessThan(0);
    expect(remainder).toBeCloseTo(-5 / 17, 5);
  });

  it("keeps a same-sign remainder (no reset)", () => {
    const { remainder } = accumulateWheelLines(5, 17, 0.5);
    expect(remainder).toBeCloseTo(0.5 + 5 / 17, 5);
  });
});

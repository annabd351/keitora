// Unit tests for the pure clampStoredNumber / parseStoredJson helpers exported from
// useLocalStorageState. The React hooks themselves (useLocalStorageNumber,
// useLocalStorageJson) are not tested here because the test environment does not
// include a DOM with localStorage; the clamp + parse logic is the only
// stateful-independent behaviour worth covering.

import { describe, expect, it } from "vitest";
import { clampStoredNumber, parseStoredJson } from "./useLocalStorageState";

describe("clampStoredNumber", () => {
  it("returns fallback when raw is null (key not in storage)", () => {
    expect(clampStoredNumber(null, 13, 8, 32)).toBe(13);
  });

  it("returns fallback when raw is not a number (corrupt storage)", () => {
    expect(clampStoredNumber("not-a-number", 13, 8, 32)).toBe(13);
    // Note: Number("") === 0 which is finite, so it clamps to min (8), not fallback.
    // Only truly non-numeric strings (NaN result) fall back.
    expect(clampStoredNumber("Infinity", 13, 8, 32)).toBe(13); // Infinity is not finite
    expect(clampStoredNumber("abc", 13, 8, 32)).toBe(13);
  });

  it("returns fallback for NaN string", () => {
    expect(clampStoredNumber("NaN", 13, 8, 32)).toBe(13);
  });

  it("returns the value when within range", () => {
    expect(clampStoredNumber("16", 13, 8, 32)).toBe(16);
    expect(clampStoredNumber("8", 13, 8, 32)).toBe(8);
    expect(clampStoredNumber("32", 13, 8, 32)).toBe(32);
  });

  it("clamps values below the minimum up to min", () => {
    expect(clampStoredNumber("5", 13, 8, 32)).toBe(8);
    expect(clampStoredNumber("0", 13, 8, 32)).toBe(8);
    expect(clampStoredNumber("-100", 13, 8, 32)).toBe(8);
  });

  it("clamps values above the maximum down to max", () => {
    expect(clampStoredNumber("40", 13, 8, 32)).toBe(32);
    expect(clampStoredNumber("999", 13, 8, 32)).toBe(32);
  });

  it("persists decimal values correctly (rounds via clamp, not floor)", () => {
    // 8.5 is within [8, 32] so it passes through as-is
    expect(clampStoredNumber("8.5", 13, 8, 32)).toBe(8.5);
  });
});

describe("parseStoredJson", () => {
  const isStrings = (v: unknown): v is string[] | null =>
    v === null || (Array.isArray(v) && v.every((x) => typeof x === "string"));
  const fallback = ["fallback"];

  it("returns the parsed value when it passes validation", () => {
    expect(parseStoredJson('["a","b"]', fallback, isStrings)).toEqual(["a", "b"]);
    expect(parseStoredJson("[]", fallback, isStrings)).toEqual([]);
  });

  it("returns a stored JSON null when the validator accepts it", () => {
    expect(parseStoredJson("null", fallback, isStrings)).toBeNull();
  });

  it("returns fallback when raw is null (key not in storage)", () => {
    expect(parseStoredJson(null, fallback, isStrings)).toBe(fallback);
  });

  it("returns fallback for malformed JSON (corrupt storage)", () => {
    expect(parseStoredJson('["a"', fallback, isStrings)).toBe(fallback);
    expect(parseStoredJson("not json", fallback, isStrings)).toBe(fallback);
    expect(parseStoredJson("", fallback, isStrings)).toBe(fallback);
  });

  it("returns fallback when the parsed value fails validation", () => {
    expect(parseStoredJson("[1,2]", fallback, isStrings)).toBe(fallback);
    expect(parseStoredJson('{"a":1}', fallback, isStrings)).toBe(fallback);
    expect(parseStoredJson('"a"', fallback, isStrings)).toBe(fallback);
    expect(parseStoredJson('["a",3]', fallback, isStrings)).toBe(fallback);
  });
});

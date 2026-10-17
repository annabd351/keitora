import { describe, expect, it } from "vitest";
import { PROJECT_HUES, projectHue, projectMonogram } from "./projectHue";

describe("projectHue", () => {
  it("is stable for a key and always one of the palette hues", () => {
    for (const key of ["keitora", "my-game", "backend", "", "日本"]) {
      const h = projectHue(key);
      expect(projectHue(key)).toBe(h);
      expect(PROJECT_HUES).toContain(h);
    }
  });

  it("spreads different keys across several hues", () => {
    const keys = Array.from({ length: 40 }, (_, i) => `project-${i}`);
    expect(new Set(keys.map(projectHue)).size).toBeGreaterThan(5);
  });
});

describe("projectMonogram", () => {
  it("is the first letter or digit, uppercased", () => {
    expect(projectMonogram("keitora")).toBe("K");
    expect(projectMonogram("_lodi")).toBe("L");
    expect(projectMonogram("2fast")).toBe("2");
    expect(projectMonogram("élan")).toBe("É");
  });

  it("falls back to ? with nothing usable", () => {
    expect(projectMonogram("")).toBe("?");
    expect(projectMonogram("--")).toBe("?");
  });
});

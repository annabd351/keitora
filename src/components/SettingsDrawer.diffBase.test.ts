import { describe, expect, it } from "vitest";
import { buildDiffBase } from "./SettingsDrawer";

describe("buildDiffBase", () => {
  it("maps auto and empty ref/forkPoint text to an explicit {kind:'auto'}", () => {
    expect(buildDiffBase("auto", "anything")).toEqual({ kind: "auto" });
    expect(buildDiffBase("ref", "   ")).toEqual({ kind: "auto" });
    expect(buildDiffBase("forkPoint", "")).toEqual({ kind: "auto" });
  });

  it("maps upstream regardless of text", () => {
    expect(buildDiffBase("upstream", "")).toEqual({ kind: "upstream" });
  });

  it("trims and carries ref name / fork-point parent", () => {
    expect(buildDiffBase("ref", "  origin/release-5.2 ")).toEqual({
      kind: "ref",
      name: "origin/release-5.2",
    });
    expect(buildDiffBase("forkPoint", "develop")).toEqual({
      kind: "forkPoint",
      parent: "develop",
    });
  });
});

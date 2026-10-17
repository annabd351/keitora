import { describe, expect, it } from "vitest";
import {
  assetMatches,
  bumpVersion,
  compareVersions,
  decideUpload,
  needsShell,
  parseArgs,
  parseSetVersion,
  parseVersion,
  platformArtifactPattern,
  tauriArchToken,
} from "./release.lib.ts";

describe("parseVersion / bumpVersion", () => {
  it("parses X.Y.Z", () => {
    expect(parseVersion("1.2.3")).toEqual([1, 2, 3]);
  });
  it("rejects malformed versions", () => {
    for (const bad of ["1.2", "x.y.z", "1.2.3.4", "1.2.3-beta", "", "v1.2.3"]) {
      expect(() => parseVersion(bad)).toThrow();
    }
  });
  it("bumps patch/minor/major with correct resets", () => {
    expect(bumpVersion("1.2.3", "patch")).toBe("1.2.4");
    expect(bumpVersion("1.2.3", "minor")).toBe("1.3.0");
    expect(bumpVersion("1.2.3", "major")).toBe("2.0.0");
    expect(bumpVersion("0.1.0", "patch")).toBe("0.1.1");
  });
});

describe("parseSetVersion / compareVersions", () => {
  it("normalizes valid, throws on invalid", () => {
    expect(parseSetVersion("2.0.0")).toBe("2.0.0");
    expect(() => parseSetVersion("2.0")).toThrow();
    expect(() => parseSetVersion("a.b.c")).toThrow();
  });
  it("compares versions", () => {
    expect(compareVersions("1.0.0", "1.0.1")).toBeLessThan(0);
    expect(compareVersions("1.2.0", "1.1.9")).toBeGreaterThan(0);
    expect(compareVersions("0.1.0", "0.1.0")).toBe(0);
  });
});

describe("tauriArchToken", () => {
  it("maps node arch to tauri token", () => {
    expect(tauriArchToken("arm64")).toBe("aarch64");
    expect(tauriArchToken("x64")).toBe("x64");
  });
  it("throws on unknown arch", () => {
    expect(() => tauriArchToken("ia32")).toThrow();
  });
});

describe("platformArtifactPattern / assetMatches", () => {
  it("matches the correct windows artifact only", () => {
    const p = platformArtifactPattern("win32", "0.1.0", "x64");
    expect(assetMatches("Keitora_0.1.0_x64-setup.exe", p)).toBe(true);
    // stale pre-rename artifact must NOT match
    expect(assetMatches("OldName_0.1.0_x64-setup.exe", p)).toBe(false);
    // wrong version / arch / kind
    expect(assetMatches("Keitora_0.2.0_x64-setup.exe", p)).toBe(false);
    expect(assetMatches("Keitora_0.1.0_aarch64-setup.exe", p)).toBe(false);
    expect(assetMatches("Keitora_0.1.0_x64.msi", p)).toBe(false);
  });
  it("matches the correct macos artifact only", () => {
    const p = platformArtifactPattern("darwin", "0.1.0", "aarch64");
    expect(assetMatches("Keitora_0.1.0_aarch64.dmg", p)).toBe(true);
    expect(assetMatches("Keitora_0.1.0_x64.dmg", p)).toBe(false);
    expect(assetMatches("Keitora_0.1.0_aarch64-setup.exe", p)).toBe(false);
  });
  it("throws on unsupported platform", () => {
    expect(() => platformArtifactPattern("linux", "0.1.0", "x64")).toThrow();
  });
});

describe("decideUpload", () => {
  it("uploads when absent", () => {
    expect(decideUpload(false, false)).toBe("upload");
    expect(decideUpload(false, true)).toBe("upload");
  });
  it("skips when present without overwrite, clobbers with overwrite", () => {
    expect(decideUpload(true, false)).toBe("skip");
    expect(decideUpload(true, true)).toBe("clobber");
  });
});

describe("needsShell", () => {
  it("only yarn needs a shell, and only on Windows", () => {
    // yarn is a .cmd shim on Windows -> needs a shell there
    expect(needsShell("yarn", "win32")).toBe(true);
    // git/gh/cargo/bash are real executables -> never a shell (would word-split spaced args)
    for (const cmd of ["git", "gh", "cargo", "bash"]) {
      expect(needsShell(cmd, "win32")).toBe(false);
    }
    // on non-Windows nothing needs a shell
    for (const cmd of ["yarn", "git", "gh", "bash"]) {
      expect(needsShell(cmd, "darwin")).toBe(false);
    }
  });
});

describe("parseArgs", () => {
  it("defaults to no bump, no flags", () => {
    expect(parseArgs([])).toEqual({ overwrite: false, dryRun: false, help: false });
  });
  it("parses a single bump", () => {
    expect(parseArgs(["--minor"]).bump).toBe("minor");
  });
  it("parses --set with a version", () => {
    expect(parseArgs(["--set", "1.4.0"]).setVersion).toBe("1.4.0");
  });
  it("parses flags", () => {
    const o = parseArgs(["--patch", "--overwrite", "--dry-run"]);
    expect(o).toMatchObject({ bump: "patch", overwrite: true, dryRun: true });
  });
  it("rejects conflicting bump flags", () => {
    expect(() => parseArgs(["--major", "--minor"])).toThrow(/Conflicting/);
  });
  it("rejects --set combined with a bump", () => {
    expect(() => parseArgs(["--set", "1.2.3", "--patch"])).toThrow(/Cannot combine/);
  });
  it("rejects --set without a value", () => {
    expect(() => parseArgs(["--set"])).toThrow();
    expect(() => parseArgs(["--set", "--dry-run"])).toThrow();
  });
  it("rejects unknown args", () => {
    expect(() => parseArgs(["--wat"])).toThrow(/Unknown/);
  });
});

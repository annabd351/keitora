import { describe, expect, it } from "vitest";
import type { DaemonSessionHint } from "./ipc";
import { countsByProject } from "./sessionCounts";

// unit tests for per-project session count helpers.

function session(project: string, worktree: string): DaemonSessionHint {
  return { project, worktree, gen: 1 };
}

describe("countsByProject", () => {
  it("returns an empty map for an empty list", () => {
    const result = countsByProject([]);
    expect(result.size).toBe(0);
  });

  it("groups sessions from the same project", () => {
    const sessions = [session("alpha", "sayo"), session("alpha", "sol")];
    const result = countsByProject(sessions);
    expect(result.size).toBe(1);
    expect(result.get("alpha")).toBe(2);
  });

  it("counts independently when two projects share the same worktree name", () => {
    // keitora + tnk both have a 'sayo' worktree — they must count independently.
    const sessions = [session("alpha", "sayo"), session("beta", "sayo"), session("beta", "sol")];
    const result = countsByProject(sessions);
    expect(result.size).toBe(2);
    expect(result.get("alpha")).toBe(1);
    expect(result.get("beta")).toBe(2);
  });

  it("handles a single session", () => {
    const result = countsByProject([session("only", "main")]);
    expect(result.size).toBe(1);
    expect(result.get("only")).toBe(1);
  });
});

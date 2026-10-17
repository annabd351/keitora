import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectSummary } from "./ipc";

// Mock the IPC layer so switchProject's `setActiveProject` round-trip is a spy
// (no Tauri runtime in vitest). The mock factory is hoisted, so the spy is
// declared with `vi.hoisted` to keep it referenceable from both here and the
// factory.
const { setActiveProjectMock } = vi.hoisted(() => ({
  setActiveProjectMock: vi.fn<(name: string) => Promise<void>>(),
}));

vi.mock("./ipc", () => ({
  setActiveProject: setActiveProjectMock,
}));

import { switchProject, switchTarget } from "./switchProject";

const projA: ProjectSummary = {
  name: "a",
  resolved_name: "a",
  root: "/a",
  active: true,
  db_path: "/a/tasks.db",
  worktrees: [],
  backend: "sqlite",
};
const projB: ProjectSummary = {
  name: "b",
  resolved_name: "b",
  root: "/b",
  active: false,
  db_path: "/b/tasks.db",
  worktrees: [],
  backend: "sqlite",
};

describe("switchProject", () => {
  beforeEach(() => {
    setActiveProjectMock.mockReset();
  });

  it("optimistically sets project before the RPC resolves", async () => {
    // setActiveProject resolves after one microtask tick.
    setActiveProjectMock.mockReturnValue(Promise.resolve());
    const setProject = vi.fn();
    const showToast = vi.fn();

    const p = switchProject("b", {
      setProject,
      currentProject: projA,
      showToast,
      summary: projB,
    });

    // The optimistic flip happens synchronously, before awaiting the RPC.
    expect(setProject).toHaveBeenCalledTimes(1);
    expect(setProject).toHaveBeenCalledWith(projB);
    expect(showToast).not.toHaveBeenCalled();

    expect(await p).toBe(true); // resolves true on success
    expect(setActiveProjectMock).toHaveBeenCalledWith("b");
    expect(showToast).toHaveBeenCalledWith("Switched to b.");
    // No revert: setProject called exactly once (the optimistic flip).
    expect(setProject).toHaveBeenCalledTimes(1);
  });

  it("reverts the optimistic flip on RPC error", async () => {
    setActiveProjectMock.mockRejectedValue(new Error("fail"));
    const setProject = vi.fn();
    const showToast = vi.fn();

    const ok = await switchProject("b", {
      setProject,
      currentProject: projA,
      showToast,
      summary: projB,
    });

    expect(ok).toBe(false); // resolves false so the caller can revert its own state
    // First the optimistic flip (projB), then the revert (back to projA).
    expect(setProject).toHaveBeenCalledTimes(2);
    expect(setProject).toHaveBeenNthCalledWith(1, projB);
    expect(setProject).toHaveBeenNthCalledWith(2, projA);
    expect(showToast).toHaveBeenCalledWith("Error: fail");
  });

  it("does not flip optimistically without a summary (Settings degraded path)", async () => {
    setActiveProjectMock.mockReturnValue(Promise.resolve());
    const setProject = vi.fn();
    const showToast = vi.fn();

    await switchProject("b", {
      setProject,
      currentProject: projA,
      showToast,
      // no summary
    });

    expect(setProject).not.toHaveBeenCalled();
    expect(setActiveProjectMock).toHaveBeenCalledWith("b");
    expect(showToast).toHaveBeenCalledWith("Switched to b.");
  });

  it("does not revert on error when there was no optimistic summary", async () => {
    setActiveProjectMock.mockRejectedValue(new Error("boom"));
    const setProject = vi.fn();
    const showToast = vi.fn();

    await switchProject("b", {
      setProject,
      currentProject: projA,
      showToast,
    });

    expect(setProject).not.toHaveBeenCalled();
    expect(showToast).toHaveBeenCalledWith("Error: boom");
  });
});

describe("switchProject silent", () => {
  beforeEach(() => {
    setActiveProjectMock.mockReset();
  });

  it("skips the success toast", async () => {
    setActiveProjectMock.mockReturnValue(Promise.resolve());
    const showToast = vi.fn();

    const ok = await switchProject("b", {
      setProject: vi.fn(),
      currentProject: projA,
      showToast,
      summary: projB,
      silent: true,
    });

    expect(ok).toBe(true);
    expect(setActiveProjectMock).toHaveBeenCalledWith("b");
    expect(showToast).not.toHaveBeenCalled();
  });

  it("still shows the error toast", async () => {
    setActiveProjectMock.mockRejectedValue(new Error("nope"));
    const showToast = vi.fn();

    const ok = await switchProject("b", {
      setProject: vi.fn(),
      currentProject: projA,
      showToast,
      summary: projB,
      silent: true,
    });

    expect(ok).toBe(false);
    expect(showToast).toHaveBeenCalledWith("Error: nope");
  });
});

describe("switchTarget", () => {
  // Registry key "c" whose [project].name resolves to something else.
  const projC: ProjectSummary = { ...projB, name: "c", resolved_name: "c-resolved", root: "/c" };

  it("uses the caller's summary and its resolved name", () => {
    const t = switchTarget("c", projC, []);
    expect(t.summary).toBe(projC);
    expect(t.resolvedName).toBe("c-resolved");
  });

  it("falls back to the registry entry for the key", () => {
    const t = switchTarget("c", undefined, [projA, projC]);
    expect(t.summary).toBe(projC);
    expect(t.resolvedName).toBe("c-resolved");
  });

  it("flips optimistically to a summary named by the resolved name", () => {
    const t = switchTarget("c", projC, []);
    expect(t.optimistic).toEqual({ ...projC, name: "c-resolved" });
    // The caller's summary is not mutated.
    expect(projC.name).toBe("c");
  });

  it("keeps the key as the name when key and resolved name agree", () => {
    const t = switchTarget("b", projB, []);
    expect(t.resolvedName).toBe("b");
    expect(t.optimistic).toEqual(projB);
  });

  it("targets the bare key with no optimistic flip for an unknown project", () => {
    const t = switchTarget("zzz", undefined, [projA]);
    expect(t.summary).toBeUndefined();
    expect(t.resolvedName).toBe("zzz");
    expect(t.optimistic).toBeUndefined();
  });
});

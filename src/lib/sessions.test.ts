import { describe, expect, it } from "vitest";
import { daemonStats } from "./ipc";
import { shouldSeedFromCache } from "./projectScope";
import type { WorktreeSession } from "./sessions";
import {
  DAEMON_EVENTS_ONLY,
  daemonStatsLoader,
  rowsForProject,
  SessionCache,
  sessionsByWorktree,
} from "./sessions";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

function makeRow(
  project: string,
  worktree: string,
  state: WorktreeSession["state"] = "idle",
): WorktreeSession {
  return {
    project,
    worktree,
    state,
    current_task_id: null,
    pid: null,
    last_heartbeat: "2024-01-01T00:00:00Z",
  };
}

const ROW_A1 = makeRow("projA", "main", "idle");
const ROW_A2 = makeRow("projA", "feature", "busy");
const ROW_B1 = makeRow("projB", "main", "waiting");
const ROW_B2 = makeRow("projB", "hotfix", "offline");

// ---------------------------------------------------------------------------
// rowsForProject
// ---------------------------------------------------------------------------

describe("rowsForProject", () => {
  it("keeps only rows whose project matches", () => {
    const result = rowsForProject([ROW_A1, ROW_A2, ROW_B1, ROW_B2], "projA");
    expect(result).toEqual([ROW_A1, ROW_A2]);
  });

  it("returns [] when no rows match the project", () => {
    const result = rowsForProject([ROW_B1, ROW_B2], "projA");
    expect(result).toEqual([]);
  });

  it("returns [] for an empty input array", () => {
    expect(rowsForProject([], "projA")).toEqual([]);
  });

  it("returns [] for a null project", () => {
    expect(rowsForProject([ROW_A1, ROW_B1], null)).toEqual([]);
  });

  it("returns [] for an undefined project", () => {
    expect(rowsForProject([ROW_A1, ROW_B1], undefined)).toEqual([]);
  });

  it("returns [] for an empty-string project", () => {
    expect(rowsForProject([ROW_A1, ROW_B1], "")).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// sessionsByWorktree
// ---------------------------------------------------------------------------

describe("sessionsByWorktree", () => {
  it("keys each row by its worktree name", () => {
    const map = sessionsByWorktree([ROW_A1, ROW_A2]);
    expect(map.get("main")).toEqual(ROW_A1);
    expect(map.get("feature")).toEqual(ROW_A2);
    expect(map.size).toBe(2);
  });

  it("returns an empty map for an empty input", () => {
    expect(sessionsByWorktree([]).size).toBe(0);
  });

  it("last-wins when duplicate worktree names appear", () => {
    // Later entry for the same worktree name overwrites the earlier one.
    const earlier = makeRow("projA", "main", "idle");
    const later = makeRow("projA", "main", "busy");
    const map = sessionsByWorktree([earlier, later]);
    expect(map.size).toBe(1);
    expect(map.get("main")).toEqual(later);
  });
});

// ---------------------------------------------------------------------------
// SessionCache
// ---------------------------------------------------------------------------

describe("SessionCache", () => {
  it("returns undefined for a project that has never been set", () => {
    const cache = new SessionCache();
    expect(cache.get("projA")).toBeUndefined();
  });

  it("round-trips: set then get returns the same entries", () => {
    const cache = new SessionCache();
    const map = sessionsByWorktree([ROW_A1, ROW_A2]);
    cache.set("projA", map);
    const retrieved = cache.get("projA");
    expect(retrieved).toBeDefined();
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    expect(retrieved!.get("main")).toEqual(ROW_A1);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    expect(retrieved!.get("feature")).toEqual(ROW_A2);
  });

  it("second set overwrites the first for the same project", () => {
    const cache = new SessionCache();
    cache.set("projA", sessionsByWorktree([ROW_A1]));
    cache.set("projA", sessionsByWorktree([ROW_A2]));
    const retrieved = cache.get("projA");
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    expect(retrieved!.size).toBe(1);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    expect(retrieved!.get("feature")).toEqual(ROW_A2);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    expect(retrieved!.get("main")).toBeUndefined();
  });

  it("cross-project isolation: setting projA does not affect projB", () => {
    const cache = new SessionCache();
    cache.set("projA", sessionsByWorktree([ROW_A1, ROW_A2]));
    expect(cache.get("projB")).toBeUndefined();
  });

  it("cross-project isolation: setting projB does not affect projA", () => {
    const cache = new SessionCache();
    cache.set("projA", sessionsByWorktree([ROW_A1]));
    cache.set("projB", sessionsByWorktree([ROW_B1]));
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    const a = cache.get("projA")!;
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    const b = cache.get("projB")!;
    expect(a.get("main")).toEqual(ROW_A1);
    expect(b.get("main")).toEqual(ROW_B1);
    // projA doesn't expose projB's rows
    expect(a.get("hotfix")).toBeUndefined();
    // projB doesn't expose projA's rows
    expect(b.get("feature")).toBeUndefined();
  });

  it("stored copy is isolated: mutating the original map does not corrupt the cache", () => {
    const cache = new SessionCache();
    const original = sessionsByWorktree([ROW_A1]);
    cache.set("projA", original);
    // Mutate the original after storing.
    original.set("sneaky", makeRow("projA", "sneaky", "busy"));
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    const retrieved = cache.get("projA")!;
    expect(retrieved.get("sneaky")).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// Combined: filter + cache never surfaces project B rows when active is A
// ---------------------------------------------------------------------------

describe("rowsForProject + SessionCache integration", () => {
  it("never surfaces project B rows when active project is A", () => {
    const allRows = [ROW_A1, ROW_A2, ROW_B1, ROW_B2];
    const cache = new SessionCache();

    // Simulate a load for projA.
    const projARows = rowsForProject(allRows, "projA");
    const projAMap = sessionsByWorktree(projARows);
    cache.set("projA", projAMap);

    // Simulate a load for projB.
    const projBRows = rowsForProject(allRows, "projB");
    const projBMap = sessionsByWorktree(projBRows);
    cache.set("projB", projBMap);

    // When active project is A, retrieved map must contain only A rows.
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence; value is guaranteed by preceding cache.set call
    const fromCache = cache.get("projA")!;
    expect(fromCache.get("main")).toEqual(ROW_A1);
    expect(fromCache.get("feature")).toEqual(ROW_A2);
    // B-only worktrees must not appear.
    expect(fromCache.get("hotfix")).toBeUndefined();

    // The rowsForProject filter itself also drops B rows when querying for A.
    const liveFiltered = rowsForProject(allRows, "projA");
    for (const row of liveFiltered) {
      expect(row.project).toBe("projA");
    }
  });

  it("a late pre-switch response (rows tagged with old project) is dropped by the filter", () => {
    // Simulate: active project is now "projB", but a stale sessions_list response
    // arrives carrying "projA" rows. rowsForProject must drop them all.
    const staleRows = [ROW_A1, ROW_A2];
    const visible = rowsForProject(staleRows, "projB");
    expect(visible).toEqual([]);
    expect(sessionsByWorktree(visible).size).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// shouldSeedFromCache — seed only before a project's first real load
// ---------------------------------------------------------------------------

describe("shouldSeedFromCache", () => {
  it("seeds when empty and the project hasn't loaded yet (switch-back window)", () => {
    // filtered empty, last loaded project was a DIFFERENT one → seed from cache.
    expect(shouldSeedFromCache(0, "projB", "projA")).toBe(true);
    // never loaded anything yet.
    expect(shouldSeedFromCache(0, null, "projA")).toBe(true);
  });

  it("does NOT seed once the active project has had a real load (genuine empty)", () => {
    // This is the #2 fix: a project that loaded real data and is now empty must
    // show empty, not stale cached lights.
    expect(shouldSeedFromCache(0, "projA", "projA")).toBe(false);
  });

  it("does NOT seed when the filtered map is non-empty (live data wins)", () => {
    expect(shouldSeedFromCache(2, "projB", "projA")).toBe(false);
    expect(shouldSeedFromCache(2, "projA", "projA")).toBe(false);
  });

  it("does NOT seed when there is no active project", () => {
    expect(shouldSeedFromCache(0, null, null)).toBe(false);
    expect(shouldSeedFromCache(0, "projA", null)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Daemon status / stats subscriptions
// ---------------------------------------------------------------------------

describe("daemon subscriptions", () => {
  it("status and stats have no resync timer", () => {
    expect(DAEMON_EVENTS_ONLY.intervalMs).toBe(0);
  });

  it("disabled stats never ask the backend", async () => {
    const loader = daemonStatsLoader(false);
    expect(loader).not.toBe(daemonStats);
    await expect(loader()).resolves.toBeNull();
  });

  it("stats loaders are stable identities", () => {
    expect(daemonStatsLoader(true)).toBe(daemonStats);
    expect(daemonStatsLoader(false)).toBe(daemonStatsLoader(false));
  });
});

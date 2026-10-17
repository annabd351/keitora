import { describe, expect, it } from "vitest";
import { laneStatusText } from "./LaneHeader";
import type { Lane } from "./useBoardLanes";

// Minimal Lane factory — only the fields laneStatusText cares about.
function mkLane(overrides: Partial<Lane>): Lane {
  return {
    key: "test",
    kind: "worktree",
    label: "test",
    tasks: [],
    count: 0,
    isEmptyOffline: false,
    ...overrides,
  };
}

describe("laneStatusText", () => {
  it('returns "unassigned · drag to a worktree" for unassigned lane', () => {
    expect(laneStatusText(mkLane({ kind: "unassigned", key: "open" }))).toBe(
      "unassigned · drag to a worktree",
    );
  });

  it('returns "completed" for done lane', () => {
    expect(laneStatusText(mkLane({ kind: "done", key: "done" }))).toBe("completed");
  });

  it('returns "idle · ready" for idle worktree', () => {
    expect(
      laneStatusText(
        mkLane({
          session: {
            project: "p",
            worktree: "test",
            state: "idle",
            current_task_id: null,
            pid: null,
            last_heartbeat: "",
          },
        }),
      ),
    ).toBe("idle · ready");
  });

  it('returns "busy" for busy worktree with no queue', () => {
    expect(
      laneStatusText(
        mkLane({
          session: {
            project: "p",
            worktree: "test",
            state: "busy",
            current_task_id: null,
            pid: null,
            last_heartbeat: "",
          },
          tasks: [],
        }),
      ),
    ).toBe("busy");
  });

  it('returns "busy · N queued" for busy worktree with queued tasks', () => {
    const lane = mkLane({
      key: "mywt",
      session: {
        project: "p",
        worktree: "mywt",
        state: "busy",
        current_task_id: null,
        pid: null,
        last_heartbeat: "",
      },
      tasks: [
        // reserved (open + assigned to this worktree)
        {
          id: 1,
          title: "t",
          status: "open",
          assigned_to: "mywt",
          priority: "normal",
          tags: [],
          blocked_by: [],
          description: "",
        } as unknown as Lane["tasks"][0],
        {
          id: 2,
          title: "t",
          status: "open",
          assigned_to: "mywt",
          priority: "normal",
          tags: [],
          blocked_by: [],
          description: "",
        } as unknown as Lane["tasks"][0],
        // not reserved
        {
          id: 3,
          title: "t",
          status: "active",
          assigned_to: "mywt",
          priority: "normal",
          tags: [],
          blocked_by: [],
          description: "",
        } as unknown as Lane["tasks"][0],
      ],
    });
    expect(laneStatusText(lane)).toBe("busy · 2 queued");
  });

  it('returns "waiting · N queued" for waiting worktree with queued tasks', () => {
    const lane = mkLane({
      key: "mywt",
      session: {
        project: "p",
        worktree: "mywt",
        state: "waiting",
        current_task_id: null,
        pid: null,
        last_heartbeat: "",
      },
      tasks: [
        {
          id: 1,
          title: "t",
          status: "open",
          assigned_to: "mywt",
          priority: "normal",
          tags: [],
          blocked_by: [],
          description: "",
        } as unknown as Lane["tasks"][0],
      ],
    });
    expect(laneStatusText(lane)).toBe("waiting · 1 queued");
  });

  it('returns "offline" when session is absent', () => {
    expect(laneStatusText(mkLane({ session: undefined }))).toBe("offline");
  });

  it('returns "offline" when session state is offline', () => {
    expect(
      laneStatusText(
        mkLane({
          session: {
            project: "p",
            worktree: "test",
            state: "offline",
            current_task_id: null,
            pid: null,
            last_heartbeat: "",
          },
        }),
      ),
    ).toBe("offline");
  });
});

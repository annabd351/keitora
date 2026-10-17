import type { UnlistenFn } from "@tauri-apps/api/event";
import { describe, expect, it, vi } from "vitest";
import {
  HEALTHY_RECOVERY,
  type RecoveryInfo,
  type RecoveryPhase,
  type RecoveryQueueItem,
} from "./ipc";
import {
  bannerCount,
  bannerResumeLabel,
  bannerVisible,
  canRestartNow,
  paneRecoveryState,
  RESTART_FALLBACK_MS,
  recoveryItem,
  recoveryPaneMessage,
  restartOutcome,
  showRailRestart,
} from "./recovery";
import { type SubscribeResult, subscribeLive } from "./useLiveResource";

vi.mock("./transport", () => ({
  getTransport: () => ({ invoke: vi.fn(), listen: vi.fn() }),
}));

function info(over: Partial<RecoveryInfo>): RecoveryInfo {
  return { ...HEALTHY_RECOVERY, ...over };
}

function item(
  worktree: string,
  state: RecoveryQueueItem["state"],
  project = "p",
): RecoveryQueueItem {
  return { project, worktree, state };
}

const PHASES: RecoveryPhase[] = [
  "Healthy",
  "Down",
  "AwaitHeadroom",
  "Restarting",
  "Reconciling",
  "Resuming",
  "Paused",
];

describe("paneRecoveryState", () => {
  it("is suspended for a suspended item in any phase", () => {
    for (const phase of PHASES) {
      const i = info({ phase, queue: [item("w", "suspended")] });
      expect(paneRecoveryState(i, "p", "w")).toBe("suspended");
    }
  });

  it("is recovering in every other phase, queued or not", () => {
    for (const phase of PHASES.filter((p) => p !== "Healthy")) {
      expect(paneRecoveryState(info({ phase }), "p", "unknown")).toBe("recovering");
      const queued = info({ phase, queue: [item("w", "queued")] });
      expect(paneRecoveryState(queued, "p", "w")).toBe("recovering");
    }
  });

  it("is nothing while healthy without a suspended item", () => {
    expect(paneRecoveryState(HEALTHY_RECOVERY, "p", "w")).toBeNull();
    const other = info({ queue: [item("w", "suspended", "other")] });
    expect(paneRecoveryState(other, "p", "w")).toBeNull();
  });

  it("is nothing for a settled session in every phase but Paused", () => {
    for (const phase of PHASES.filter((p) => p !== "Paused")) {
      const i = info({ phase, settled: [{ project: "p", worktree: "w" }] });
      expect(paneRecoveryState(i, "p", "w")).toBeNull();
    }
  });

  it("keeps failed, queued, in-flight and other keys recovering while resuming", () => {
    const i = info({
      phase: "Resuming",
      queue: [item("q", "queued"), item("r", "resuming")],
      settled: [{ project: "p", worktree: "w" }],
    });
    expect(paneRecoveryState(i, "p", "q")).toBe("recovering");
    expect(paneRecoveryState(i, "p", "r")).toBe("recovering");
    expect(paneRecoveryState(i, "p", "failed")).toBe("recovering");
    expect(paneRecoveryState(i, "other", "w")).toBe("recovering");
  });

  it("keeps a settled session recovering while paused", () => {
    const i = info({ phase: "Paused", settled: [{ project: "p", worktree: "w" }] });
    expect(paneRecoveryState(i, "p", "w")).toBe("recovering");
  });

  it("keeps a settled session live when the queue falls back to waiting for memory", () => {
    const i = info({
      phase: "AwaitHeadroom",
      queue: [item("q", "suspended")],
      settled: [{ project: "p", worktree: "w" }],
    });
    expect(paneRecoveryState(i, "p", "w")).toBeNull();
    expect(paneRecoveryState(i, "p", "q")).toBe("suspended");
    expect(paneRecoveryState(i, "p", "other")).toBe("recovering");
  });

  it("lets a suspended item win over settled", () => {
    const i = info({
      phase: "Resuming",
      queue: [item("w", "suspended")],
      settled: [{ project: "p", worktree: "w" }],
    });
    expect(paneRecoveryState(i, "p", "w")).toBe("suspended");
  });
});

describe("recoveryPaneMessage", () => {
  it("names each documented case", () => {
    expect(recoveryPaneMessage(info({ phase: "Resuming" }), item("w", "resuming"))).toBe(
      "Resuming your conversation…",
    );
    expect(recoveryPaneMessage(info({ phase: "Resuming" }), item("w", "queued"))).toBe(
      "Waiting for its turn to resume…",
    );
    for (const phase of ["Down", "AwaitHeadroom"] as const) {
      expect(recoveryPaneMessage(info({ phase }), undefined)).toBe(
        "Waiting for memory — your session will resume automatically",
      );
    }
    for (const phase of ["Restarting", "Reconciling"] as const) {
      expect(recoveryPaneMessage(info({ phase }), undefined)).toBe(
        "herdr is restarting — your session will resume automatically",
      );
    }
    expect(recoveryPaneMessage(info({ phase: "Paused" }), undefined)).toBe(
      "Recovery paused — use “Resume now” above",
    );
    expect(recoveryPaneMessage(info({ phase: "Resuming" }), undefined)).toBe("Recovering…");
  });
});

describe("showRailRestart", () => {
  it("shows only for a restartable offline daemon", () => {
    expect(showRailRestart({ state: "Disconnected", restartable: true })).toBe(true);
    expect(showRailRestart({ state: "Disconnected", restartable: false })).toBe(false);
    expect(showRailRestart({ state: "Connected", restartable: true })).toBe(false);
    expect(showRailRestart({ state: "NeverStarted", restartable: true })).toBe(false);
  });
});

describe("canRestartNow", () => {
  it("allows a restart in every idle phase", () => {
    for (const phase of ["Healthy", "Down", "AwaitHeadroom", "Paused"] as const) {
      expect(canRestartNow({ phase }, false)).toBe(true);
    }
  });

  it("refuses while recovery restarts the server or resumes sessions", () => {
    for (const phase of ["Restarting", "Reconciling", "Resuming"] as const) {
      expect(canRestartNow({ phase }, false)).toBe(false);
    }
  });

  it("refuses in any phase while a request is in flight", () => {
    const phases = [
      "Healthy",
      "Down",
      "AwaitHeadroom",
      "Restarting",
      "Reconciling",
      "Resuming",
      "Paused",
    ] as const;
    for (const phase of phases) {
      expect(canRestartNow({ phase }, true)).toBe(false);
    }
  });
});

describe("bannerVisible", () => {
  it("stays hidden while herdr is not found, even with suspended sessions", () => {
    const notFound = { reason: "herdr was not found.", reason_code: "herdr_not_found" };
    expect(bannerVisible(info(notFound))).toBe(false);
    expect(bannerVisible(info({ ...notFound, queue: [item("w", "suspended")] }))).toBe(false);
  });

  it("shows while not healthy or while sessions wait suspended", () => {
    expect(bannerVisible(HEALTHY_RECOVERY)).toBe(false);
    expect(bannerVisible(info({ queue: [item("w", "queued")] }))).toBe(false);
    expect(bannerVisible(info({ queue: [item("w", "suspended")] }))).toBe(true);
    for (const phase of PHASES.filter((p) => p !== "Healthy")) {
      expect(bannerVisible(info({ phase }))).toBe(true);
    }
  });
});

describe("bannerResumeLabel", () => {
  it("has three labels", () => {
    expect(bannerResumeLabel(info({ phase: "AwaitHeadroom" }))).toBe(
      "Resume now (ignore memory check)",
    );
    expect(bannerResumeLabel(info({ phase: "Paused", reason_code: "headroom_cap" }))).toBe(
      "Resume now (ignore memory check)",
    );
    expect(bannerResumeLabel(info({ queue: [item("w", "suspended")] }))).toBe("Resume all");
    expect(bannerResumeLabel(info({ phase: "Paused", reason_code: "crash_loop_early" }))).toBe(
      "Resume now",
    );
    expect(bannerResumeLabel(info({ phase: "Down" }))).toBe("Resume now");
  });
});

describe("bannerCount", () => {
  it("counts finished resumes only while resuming", () => {
    expect(bannerCount(info({ phase: "Resuming", resumed: 2, failed: 1, total: 5 }))).toBe("3 / 5");
    expect(bannerCount(info({ phase: "Down", total: 5 }))).toBeNull();
  });
});

describe("recoveryItem", () => {
  it("finds the pane's entry by project and worktree", () => {
    const i = info({ queue: [item("w", "queued", "other"), item("w", "resuming")] });
    expect(recoveryItem(i, "p", "w")?.state).toBe("resuming");
    expect(recoveryItem(i, "p", "x")).toBeUndefined();
  });
});

describe("restartOutcome", () => {
  const base = { startPid: 100, startSeq: 4, pid: 100, elapsedMs: 0 };

  it("needs Healthy and a new pid for success", () => {
    expect(restartOutcome({ ...base, recovery: info({ seq: 9 }) })).toEqual({
      kind: "pending",
    });
    expect(
      restartOutcome({ ...base, pid: 200, recovery: info({ phase: "Resuming", seq: 9 }) }),
    ).toEqual({ kind: "pending" });
    expect(restartOutcome({ ...base, pid: 200, recovery: info({ seq: 9 }) })).toEqual({
      kind: "success",
    });
  });

  it("fails on Down or Paused seen after the call", () => {
    for (const phase of ["Down", "Paused"] as const) {
      expect(
        restartOutcome({ ...base, recovery: info({ phase, reason: "nope", seq: 5 }) }),
      ).toEqual({ kind: "failure", reason: "nope" });
    }
  });

  it("ignores a Down snapshot from before the call", () => {
    expect(restartOutcome({ ...base, recovery: info({ phase: "Down", seq: 4 }) })).toEqual({
      kind: "pending",
    });
  });

  it("reports a slow restart after the fallback", () => {
    expect(
      restartOutcome({
        ...base,
        elapsedMs: RESTART_FALLBACK_MS,
        recovery: info({ phase: "Restarting", seq: 6 }),
      }),
    ).toEqual({ kind: "slow" });
  });
});

describe("recovery subscription", () => {
  it("is healthy before the first load, then follows daemon://recovery", async () => {
    let handler: ((e: { payload: unknown }) => void) | undefined;
    let attach: ((u: UnlistenFn) => void) | undefined;
    const listen = vi.fn((_name: string, h: (e: { payload: unknown }) => void) => {
      handler = h;
      return new Promise<UnlistenFn>((resolve) => {
        attach = resolve;
      });
    }) as unknown as typeof import("@tauri-apps/api/event").listen;
    const snapshots = [info({ seq: 1 }), info({ phase: "Down", reason: "gone", seq: 2 })];
    const load = vi.fn(async () => snapshots.shift() as RecoveryInfo);
    let current: RecoveryInfo | null = null;
    const view = () => current ?? HEALTHY_RECOVERY;

    const stop = subscribeLive(
      load,
      "daemon://recovery",
      (r: SubscribeResult<RecoveryInfo>) => {
        if ("data" in r) current = r.data;
      },
      { listen, intervalMs: 0 },
    );
    expect(listen).toHaveBeenCalledWith("daemon://recovery", expect.any(Function));
    expect(view()).toEqual(HEALTHY_RECOVERY);

    attach?.(vi.fn() as unknown as UnlistenFn);
    await vi.waitFor(() => expect(load).toHaveBeenCalledTimes(1));
    await vi.waitFor(() => expect(view().seq).toBe(1));

    handler?.({ payload: info({ phase: "Down", seq: 2 }) });
    await vi.waitFor(() => expect(view().phase).toBe("Down"));
    expect(view().reason).toBe("gone");
    stop();
  });
});

// Pure helpers for the herdr server recovery UI: what the recovery banner and
// each pane show while herdr is being recovered, and how Settings decides that a
// requested restart finished.

import type { DaemonInfo, RecoveryInfo, RecoveryPhase, RecoveryQueueItem } from "./ipc";

/** A pane's recovery state: waiting to be resumed, parked until the user asks, or neither. */
export type PaneRecovery = "recovering" | "suspended" | null;

/** The queue entry for `project`/`worktree`, if recovery owes it a resume. */
export function recoveryItem(
  info: RecoveryInfo,
  project: string,
  worktree: string,
): RecoveryQueueItem | undefined {
  return info.queue.find((i) => i.project === project && i.worktree === worktree);
}

/**
 * The pane's recovery state. A suspended entry wins in any phase (including
 * leftovers while Healthy). A session that already resumed successfully this
 * episode (`settled`) is live, so its pane is not recovering, also when the rest
 * of the queue falls back to waiting for memory; only a pause takes it away (the
 * backend empties `settled` on every death, boot, restart and stop). Otherwise
 * any phase but Healthy is `recovering`, also for a pane recovery has not queued
 * (yet) and for one whose resume failed.
 */
export function paneRecoveryState(
  info: RecoveryInfo,
  project: string,
  worktree: string,
): PaneRecovery {
  if (recoveryItem(info, project, worktree)?.state === "suspended") return "suspended";
  if (
    info.phase !== "Paused" &&
    info.settled.some((s) => s.project === project && s.worktree === worktree)
  ) {
    return null;
  }
  return info.phase !== "Healthy" ? "recovering" : null;
}

/** What a recovering pane says about its session. */
export function recoveryPaneMessage(
  info: RecoveryInfo,
  item: RecoveryQueueItem | undefined,
): string {
  if (item?.state === "resuming") return "Resuming your conversation…";
  if (item?.state === "queued") return "Waiting for its turn to resume…";
  if (!item && (info.phase === "Down" || info.phase === "AwaitHeadroom")) {
    return "Waiting for memory — your session will resume automatically";
  }
  if (info.phase === "Restarting" || info.phase === "Reconciling") {
    return "herdr is restarting — your session will resume automatically";
  }
  if (info.phase === "Paused") return "Recovery paused — use “Resume now” above";
  return "Recovering…";
}

/** Recovery's reason code while the herdr executable cannot be found. */
export const HERDR_NOT_FOUND = "herdr_not_found";

/**
 * The banner shows while recovery is under way or sessions wait suspended. Not while herdr
 * cannot be found: the phase stays Healthy there, nothing is being recovered, "Resume all"
 * cannot work, and the rail's offline card already carries the message and a Restart button.
 */
export function bannerVisible(info: RecoveryInfo): boolean {
  if (info.reason_code === HERDR_NOT_FOUND) return false;
  return info.phase !== "Healthy" || info.queue.some((i) => i.state === "suspended");
}

/** The banner button's label. */
export function bannerResumeLabel(info: RecoveryInfo): string {
  if (info.reason_code === "headroom_cap" || info.phase === "AwaitHeadroom") {
    return "Resume now (ignore memory check)";
  }
  if (info.phase === "Healthy") return "Resume all";
  return "Resume now";
}

/** The banner's progress count while resuming (`done / total`), else `null`. */
export function bannerCount(info: RecoveryInfo): string | null {
  if (info.phase !== "Resuming") return null;
  return `${info.resumed + info.failed} / ${info.total}`;
}

/** How long Settings waits for a restart before saying it is slow. */
export const RESTART_FALLBACK_MS = 120_000;

/** Where a Settings-initiated restart stands. */
export type RestartOutcome =
  | { kind: "pending" }
  | { kind: "success" }
  | { kind: "failure"; reason: string | null }
  | { kind: "slow" };

const FAILED: readonly RecoveryPhase[] = ["Down", "Paused"];

/**
 * Decide a restart's outcome from the state seen since the call. `startPid` and
 * `startSeq` are the daemon pid and recovery `seq` captured when the restart was
 * requested: success needs recovery `Healthy` and a different pid (a new
 * server); failure is a `Down`/`Paused` snapshot newer than the call; after
 * {@link RESTART_FALLBACK_MS} with neither, the restart is reported as slow.
 */
export function restartOutcome(args: {
  startPid: number | null;
  startSeq: number;
  pid: number | null;
  recovery: RecoveryInfo;
  elapsedMs: number;
}): RestartOutcome {
  const { startPid, startSeq, pid, recovery, elapsedMs } = args;
  if (recovery.seq > startSeq && FAILED.includes(recovery.phase)) {
    return { kind: "failure", reason: recovery.reason };
  }
  if (recovery.phase === "Healthy" && pid != null && pid !== startPid) {
    return { kind: "success" };
  }
  if (elapsedMs >= RESTART_FALLBACK_MS) return { kind: "slow" };
  return { kind: "pending" };
}

/** Phases in which the server is being restarted and its sessions resumed. */
const RESTART_RUNNING: readonly RecoveryPhase[] = ["Restarting", "Reconciling", "Resuming"];

/**
 * Whether a Restart click can start a restart now. Not while our own request is still in
 * flight (`requesting`), and not while recovery is already restarting the server or
 * resuming sessions: the backend ignores a restart then, and the request itself returns as
 * soon as the restart is handed off, long before it is over. `Healthy`, `Down`,
 * `AwaitHeadroom` and `Paused` are idle: a restart is accepted there.
 */
export function canRestartNow(recovery: Pick<RecoveryInfo, "phase">, requesting: boolean): boolean {
  return !requesting && !RESTART_RUNNING.includes(recovery.phase);
}

/** The rail's Restart button: only offline, and only when a restart can help. */
export function showRailRestart(daemon: Pick<DaemonInfo, "state" | "restartable">): boolean {
  return daemon.state === "Disconnected" && daemon.restartable;
}

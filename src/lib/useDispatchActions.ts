// Shared dispatch/release actions, lifted out of Board. `runDispatch` applies a
// dispatch (the backend decides inject-vs-queue) and reports the queued case via
// toast; `runRelease` force-unclaims a reserved task. Both refetch the board on
// success and clear any stale inline error first.
//
// Board consumes this for both the drag path and the per-card menu.

import { useCallback, useRef } from "react";
import { type DispatchOutcome, dispatchApply, releaseTask } from "./dispatch";
import { errorMessage } from "./tasks";

/** What a `dispatch_apply` outcome means for the UI. Pure + exported so the
 *  branching is unit-testable without a React renderer. */
export type DispatchEffect =
  | { kind: "injected" } // fresh inject — caller may animate the fly-in
  | { kind: "queued"; toast: string } // target busy — queued; tell the user
  | { kind: "noop" }; // idempotent re-apply — nothing was injected

export function classifyDispatch(
  outcome: DispatchOutcome,
  taskId: number,
  worktree: string,
): DispatchEffect {
  // The backend idempotency guard returns dispatched:true with reason
  // "already dispatched" on a no-op re-apply — nothing was injected.
  if (outcome.dispatched && outcome.reason !== "already dispatched") {
    return { kind: "injected" };
  }
  if (!outcome.dispatched) {
    return { kind: "queued", toast: `Queued #${taskId} — @${worktree} busy` };
  }
  return { kind: "noop" };
}

export interface DispatchActions {
  /** Apply a dispatch. `onDispatched(worktree)` fires only on a fresh inject
   *  (not a queue, not an idempotent re-apply) — Board uses it for the fly-in. */
  runDispatch: (
    worktree: string,
    taskId: number,
    onDispatched?: (worktree: string) => void,
  ) => void;
  runRelease: (taskId: number) => void;
}

export function useDispatchActions(
  reload: () => Promise<void>,
  showToast: (msg: string) => void,
  /** Inline-error setter; called with `null` at the start of each action to
   *  clear a stale error, and with the message on failure. */
  setError?: (msg: string | null) => void,
): DispatchActions {
  // Task ids with a dispatch_apply in flight. A ref (not state) so the
  // re-entrancy check is synchronous: a rapid double-click is rejected before
  // it can issue a second IPC call.
  const pendingDispatch = useRef<Set<number>>(new Set());

  const runDispatch = useCallback(
    (worktree: string, taskId: number, onDispatched?: (worktree: string) => void) => {
      if (pendingDispatch.current.has(taskId)) return;
      pendingDispatch.current.add(taskId);
      setError?.(null);
      void dispatchApply(worktree, taskId)
        .then((outcome) => {
          const effect = classifyDispatch(outcome, taskId, worktree);
          if (effect.kind === "injected") onDispatched?.(worktree);
          else if (effect.kind === "queued") showToast(effect.toast);
          return reload();
        })
        .catch((e) => {
          showToast(String(e));
          setError?.(String(e));
        })
        .finally(() => {
          pendingDispatch.current.delete(taskId);
        });
    },
    [reload, showToast, setError],
  );

  const runRelease = useCallback(
    (taskId: number) => {
      setError?.(null);
      // `releaseTask` calls the `task_release` command, which rejects with a
      // structured IpcError — unwrap it (a raw String(e) would show "[object
      // Object]"). `runDispatch` above keeps String(e): `dispatch_apply` still
      // rejects with a plain string.
      void releaseTask(taskId)
        .then(() => reload())
        .catch((e) => {
          showToast(errorMessage(e));
          setError?.(errorMessage(e));
        });
    },
    [reload, showToast, setError],
  );

  return { runDispatch, runRelease };
}

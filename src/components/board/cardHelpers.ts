// Shared board helpers used by both the side-column Lane (Open/Done) and the
// center WorktreeRow, so the drag-over affordance and per-card decorators stay
// defined in exactly one place.

import type { WorktreeSession } from "../../lib/sessions";
import type { TaskWithDeps } from "../../lib/tasks";
import type { Decision } from "../../lib/useDispatchTargets";
import type { Lane } from "./useBoardLanes";

/** Body tint + active-over emphasis for a lane's live drop decision. */
export function decisionClasses(decision: Decision | undefined, isOver: boolean): string {
  switch (decision) {
    case "dispatch":
      return isOver ? "border-accent bg-accent/[.18]" : "border-accent/60 bg-accent-dim";
    case "queue":
      return isOver
        ? "border-dashed border-accent/60 bg-bg-2"
        : "border-dashed border-accent/35 bg-bg-2";
    case "release":
      return isOver ? "border-dashed border-line bg-bg-2" : "border-dashed border-line";
    case "invalid":
      return "border-transparent opacity-90";
    default:
      return "border-transparent";
  }
}

/** Per-card decorators a worktree lane confers on its tasks. Non-worktree lanes
 *  (Open/Done) never reserve or dispatch, so both flags are false there. */
export function cardDecorators(
  t: TaskWithDeps,
  lane: Lane,
  frontReserved: Map<string, number>,
  sessions: Map<string, WorktreeSession>,
  dispatchedSet: Set<string>,
): { isFrontReserved: boolean; alreadyDispatched: boolean } {
  if (lane.kind !== "worktree") {
    return { isFrontReserved: false, alreadyDispatched: false };
  }
  const isFrontReserved =
    frontReserved.get(lane.key) === t.id && (sessions.get(lane.key)?.state ?? "offline") === "idle";
  const alreadyDispatched = dispatchedSet.has(`${t.id}:${lane.key}`);
  return { isFrontReserved, alreadyDispatched };
}

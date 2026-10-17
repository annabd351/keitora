// Single source of truth for "what happens if this card lands on that lane" —
// drives both the drag affordance (Board droppable columns) and the per-card
// "Dispatch to…" menu labels. Given a dragged/selected card + the live session
// map, decide each lane's outcome.
//
// Accent discipline (proposal §UX): SOLID accent = "act now" (dispatch);
// DASHED/dim accent = "defer" (queue/reserve). `release` is neutral; `invalid`
// is a no-op (offline worktree, blocked card, or the Done lane).

import { useMemo } from "react";
import type { WorktreeSession } from "./sessions";
import type { TaskWithDeps } from "./tasks";

export type Decision = "dispatch" | "queue" | "release" | "invalid";

/** Resolve the decision for one lane given the card and the lane's session. */
export function decideLane(
  card: TaskWithDeps,
  lane: { key: string; isWorktree: boolean },
  sessions: Map<string, WorktreeSession>,
): Decision {
  // Terminal cards never dispatch/queue/release.
  if (card.status === "done" || card.status === "cancelled") return "invalid";

  // The Open lane: only meaningful for an assigned card → release it.
  if (!lane.isWorktree) {
    // The Done column is filtered out of droppables upstream; only "open"
    // reaches here. A release is valid when the card is currently assigned.
    if (lane.key === "open") {
      return card.assigned_to ? "release" : "invalid";
    }
    return "invalid";
  }

  // Worktree lane. A blocked card is a self-invalidating payload — the backend
  // `assign` would refuse it, so every worktree lane rejects it up front.
  if (card.blocked_by.length > 0) return "invalid";

  // Dropping onto the lane the card is already reserved/active in is a no-op.
  if (card.assigned_to === lane.key) return "invalid";

  const state = sessions.get(lane.key)?.state ?? "offline";
  switch (state) {
    case "idle":
      return "dispatch";
    case "busy":
    case "waiting":
      return "queue";
    default:
      return "invalid";
  }
}

/** Per-lane decisions for the currently dragged/selected card, keyed by lane key.
 *  `null` card → an empty map (nothing in flight). */
export function useDispatchTargets(
  card: TaskWithDeps | null,
  lanes: { key: string; isWorktree: boolean }[],
  sessions: Map<string, WorktreeSession>,
): Map<string, Decision> {
  return useMemo(() => {
    const out = new Map<string, Decision>();
    if (!card) return out;
    for (const lane of lanes) {
      out.set(lane.key, decideLane(card, lane, sessions));
    }
    return out;
  }, [card, lanes, sessions]);
}

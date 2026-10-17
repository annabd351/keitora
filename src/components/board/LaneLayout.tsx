// LaneLayout — three-region board layout.
//
// Open (unassigned) full-height left column | center flex-1 scrollable
// column of WorktreeRows | Done full-height right column (only when showDone).

import type { WorktreeSession } from "../../lib/sessions";
import type { Decision } from "../../lib/useDispatchTargets";
import { Lane } from "./Lane";
import type { Lane as LaneData } from "./useBoardLanes";
import { WorktreeRow } from "./WorktreeRow";

interface LaneLayoutProps {
  lanes: LaneData[];
  decisions: Map<string, Decision>;
  dragging: boolean;
  flyIn: { id: number; worktree: string } | null;
  sessions: Map<string, WorktreeSession>;
  actor: string | null;
  selectedId: number | null;
  worktreeNames: string[];
  dispatchedSet: Set<string>;
  frontReserved: Map<string, number>;
  menuTaskId: number | null;
  onMenuOpenChange: (taskId: number, open: boolean) => void;
  onSelect: (id: number) => void;
  onAction: (fn: () => Promise<unknown>) => void;
  onDispatch: (worktree: string, taskId: number) => void;
  onRelease: (taskId: number) => void;
  /** When false (default), the Done column is not rendered. */
  showDone: boolean;
}

export function LaneLayout({
  lanes,
  decisions,
  dragging,
  flyIn,
  sessions,
  actor,
  selectedId,
  worktreeNames,
  dispatchedSet,
  frontReserved,
  menuTaskId,
  onMenuOpenChange,
  onSelect,
  onAction,
  onDispatch,
  onRelease,
  showDone,
}: LaneLayoutProps) {
  const unassignedLane = lanes.find((l) => l.kind === "unassigned");
  const worktreeLanes = lanes.filter((l) => l.kind === "worktree");
  const doneLane = lanes.find((l) => l.kind === "done");

  // Shared props for Lane (side columns) and WorktreeRow.
  const sharedProps = {
    dragging,
    flyIn,
    sessions,
    actor,
    selectedId,
    worktreeNames,
    dispatchedSet,
    frontReserved,
    menuTaskId,
    onMenuOpenChange,
    onSelect,
    onAction,
    onDispatch,
    onRelease,
  };

  // Three full-height regions.
  return (
    <div className="flex min-h-0 flex-1 gap-3.5 overflow-hidden p-3.5">
      {/* Open (unassigned) — full-height left column */}
      {unassignedLane && (
        <div className="flex-none self-stretch">
          <Lane
            lane={unassignedLane}
            decision={decisions.get(unassignedLane.key)}
            headerFlash={flyIn?.worktree === unassignedLane.key}
            {...sharedProps}
          />
        </div>
      )}

      {/* Center: worktree rows in their own bounded, scrollable panel */}
      <div className="flex min-w-0 flex-1 flex-col overflow-y-auto rounded-xl border border-line">
        {worktreeLanes.map((lane) => (
          <WorktreeRow
            key={lane.key}
            lane={lane}
            decision={decisions.get(lane.key)}
            headerFlash={flyIn?.worktree === lane.key}
            {...sharedProps}
          />
        ))}
      </div>

      {/* Done — full-height right column, only when showDone */}
      {showDone && doneLane && (
        <div className="flex-none self-stretch">
          <Lane
            lane={doneLane}
            decision={decisions.get(doneLane.key)}
            headerFlash={false}
            {...sharedProps}
          />
        </div>
      )}
    </div>
  );
}

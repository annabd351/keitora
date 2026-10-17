// WorktreeRow — full-width row for one worktree lane on the board.
// Replaces WorktreeCell. Each row is a droppable target (useDroppable id = lane.key)
// with the same decisionClasses affordance. Layout: left name column (eyebrow +
// bold name) | right: flex-wrap of compact TaskCards for all lane.tasks.
// Rows are separated by hairlines; empty+offline rows are dimmed.

import { useDroppable } from "@dnd-kit/core";
import type { WorktreeSession } from "../../lib/sessions";
import type { Decision } from "../../lib/useDispatchTargets";
import { cardDecorators, decisionClasses } from "./cardHelpers";
import { TaskCard } from "./TaskCard";
import type { Lane as LaneData } from "./useBoardLanes";

// ── Props ─────────────────────────────────────────────────────────────────────

interface WorktreeRowProps {
  lane: LaneData;
  dragging: boolean;
  decision: Decision | undefined;
  sessions: Map<string, WorktreeSession>;
  actor: string | null;
  selectedId: number | null;
  worktreeNames: string[];
  dispatchedSet: Set<string>;
  frontReserved: Map<string, number>;
  flyIn: { id: number; worktree: string } | null;
  menuTaskId: number | null;
  onMenuOpenChange: (taskId: number, open: boolean) => void;
  onSelect: (id: number) => void;
  onAction: (fn: () => Promise<unknown>) => void;
  onDispatch: (worktree: string, taskId: number) => void;
  onRelease: (taskId: number) => void;
  /** One-shot flash on the name column after a successful dispatch to this worktree. */
  headerFlash?: boolean;
}

// ── Component ─────────────────────────────────────────────────────────────────

export function WorktreeRow({
  lane,
  dragging,
  decision,
  sessions,
  actor,
  selectedId,
  worktreeNames,
  dispatchedSet,
  frontReserved,
  flyIn,
  menuTaskId,
  onMenuOpenChange,
  onSelect,
  onAction,
  onDispatch,
  onRelease,
  headerFlash = false,
}: WorktreeRowProps) {
  const { setNodeRef, isOver } = useDroppable({ id: lane.key });
  const showAffordance = dragging && decision !== undefined;

  // Dim empty+offline rows
  const dimCls = lane.isEmptyOffline ? "opacity-60" : "";

  // Normal rows show a bottom divider; a drag-over row swaps it for the accent
  // affordance border (rounded, all sides) so the two never fight over border-color.
  const stateCls = showAffordance
    ? `rounded-chip border ${decisionClasses(decision, isOver)}`
    : "border-b border-line last:border-b-0";

  function renderCard(t: LaneData["tasks"][0], fill = false) {
    const { isFrontReserved, alreadyDispatched } = cardDecorators(
      t,
      lane,
      frontReserved,
      sessions,
      dispatchedSet,
    );

    return (
      <TaskCard
        key={t.id}
        t={t}
        column={lane.key}
        isWorktreeColumn={true}
        actor={actor}
        selected={t.id === selectedId}
        worktreeNames={worktreeNames}
        sessions={sessions}
        isFrontReserved={isFrontReserved}
        alreadyDispatched={alreadyDispatched}
        flyIn={flyIn?.id === t.id ? flyIn.worktree : null}
        menuOpen={menuTaskId === t.id}
        onMenuOpenChange={(open) => onMenuOpenChange(t.id, open)}
        onSelect={() => onSelect(t.id)}
        onAction={onAction}
        onDispatch={(wt) => onDispatch(wt, t.id)}
        onRelease={() => onRelease(t.id)}
        compact={true}
        fill={fill}
      />
    );
  }

  // Horizontal row — name column left, cards right. All rows share a
  // uniform min-height so empty ("no task") worktrees line up with task rows.
  return (
    <div
      ref={setNodeRef}
      className={`flex h-[128px] shrink-0 items-stretch px-3.5 py-1.5 transition-colors ${stateCls} ${dimCls}`}
    >
      {/* Left: name column */}
      <div
        className={`w-[118px] flex-none rounded-chip px-1 py-0.5 pr-3 transition-colors ${
          headerFlash ? "bg-accent-dim" : ""
        }`}
      >
        <span className="block text-[8px] uppercase tracking-[0.14em] text-tx-3">WORKTREE</span>
        <span className="block truncate text-[15px] font-semibold text-tx-1">{lane.label}</span>
      </div>

      {/* Right: compact cards wrap within the fixed-height row; if they overflow the
          cap, the region scrolls. Each card grows in width (up to a cap) to fit its
          single-line tag row rather than wrapping tags to a second line. */}
      <div className="flex max-h-full min-w-0 flex-1 flex-wrap gap-2 overflow-y-auto">
        {lane.tasks.length > 0 ? (
          lane.tasks.map((t) => (
            <div key={t.id} className="flex flex-col min-w-[188px] max-w-[260px]">
              {renderCard(t, true)}
            </div>
          ))
        ) : (
          <span className="flex min-w-[188px] items-center justify-center rounded border border-dashed border-line/60 px-2.5 text-[10px] text-tx-3">
            no task
          </span>
        )}
      </div>
    </div>
  );
}

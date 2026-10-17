// Lane — one droppable lane: header + capped-scroll body of TaskCards.
// Replaces DroppableColumn from Board.tsx. Keeps useDroppable + the drag-over
// affordance logic (border/bg by decideLane decision) exactly as before.
// If lane.isEmptyOffline, renders header-only as a slim chip (no body).

import { useDroppable } from "@dnd-kit/core";
import { useEffect, useMemo, useState } from "react";
import type { WorktreeSession } from "../../lib/sessions";
import type { Decision } from "../../lib/useDispatchTargets";
import { cardDecorators, decisionClasses } from "./cardHelpers";
import { LaneHeader } from "./LaneHeader";
import { LaneTagFilter } from "./LaneTagFilter";
import { TaskCard } from "./TaskCard";
import { activeSelection, availableTags, filterTasksByTags } from "./tagFilter";
import type { Lane as LaneData } from "./useBoardLanes";

// ── Props ─────────────────────────────────────────────────────────────────────

interface LaneProps {
  lane: LaneData;
  dragging: boolean;
  decision: Decision | undefined;
  headerFlash: boolean;
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
}

// ── Component ─────────────────────────────────────────────────────────────────

export function Lane({
  lane,
  dragging,
  decision,
  headerFlash,
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
}: LaneProps) {
  const [collapsed, setCollapsed] = useState(false);

  // Tag filter — Open (unassigned) lane only. Ephemeral (resets on reload).
  // Options derive live from the lane's own tasks.
  const isOpenLane = lane.kind === "unassigned";
  const [selectedTags, setSelectedTags] = useState<string[]>([]);
  const tagOptions = useMemo(
    () => (isOpenLane ? availableTags(lane.tasks) : []),
    [isOpenLane, lane.tasks],
  );
  // Only tags that still exist can filter. This keeps the filter from getting stuck
  // (e.g. every Open tag disappears → the menu's Clear control unmounts, but a stale
  // selection would otherwise keep hiding everything) and stops a selection made in
  // one project from silently filtering another project's Open column after a switch
  // (the lane is not remounted across project switches).
  const activeTags = useMemo(
    () => activeSelection(selectedTags, tagOptions),
    [selectedTags, tagOptions],
  );
  // Forget pruned tags so they can't resurrect if that tag later returns to the lane.
  useEffect(() => {
    if (activeTags.length !== selectedTags.length) setSelectedTags(activeTags);
  }, [activeTags, selectedTags.length]);
  const visibleTasks = useMemo(
    () => (isOpenLane ? filterTasksByTags(lane.tasks, activeTags) : lane.tasks),
    [isOpenLane, lane.tasks, activeTags],
  );
  const filterActive = isOpenLane && activeTags.length > 0;
  const toggleTag = (tag: string) =>
    setSelectedTags((s) => (s.includes(tag) ? s.filter((x) => x !== tag) : [...s, tag]));

  // Open and worktree lanes are droppable; Done is not.
  const isDroppable = lane.key === "open" || lane.kind === "worktree";
  const { setNodeRef, isOver } = useDroppable({ id: lane.key, disabled: !isDroppable });

  const showAffordance = dragging && isDroppable && decision !== undefined;

  // Empty-offline lanes render as a slim header chip only — no body.
  if (lane.isEmptyOffline) {
    return (
      <div className="flex min-h-0 w-[218px] flex-none flex-col">
        <LaneHeader
          lane={lane}
          dragging={dragging}
          decision={decision}
          headerFlash={headerFlash}
          collapsed={true}
          onToggleCollapse={() => {
            /* slim chips don't collapse further */
          }}
        />
      </div>
    );
  }

  // Lane now renders only the Open + Done side columns (worktree lanes use
  // WorktreeCell). A full-height column (fills its self-stretch wrapper, body
  // scrolls). 224px floor, but the lane fits its content so a card whose
  // single-line tag row stretches wider than the floor widens the column
  // (capped) instead of clipping the tags.

  return (
    <div className="flex h-full min-h-0 w-fit min-w-[224px] max-w-[272px] flex-col">
      <LaneHeader
        lane={lane}
        dragging={dragging}
        decision={decision}
        headerFlash={headerFlash}
        collapsed={collapsed}
        onToggleCollapse={lane.kind === "done" ? () => setCollapsed((c) => !c) : undefined}
        filterSlot={
          isOpenLane ? (
            <LaneTagFilter
              tags={tagOptions}
              selected={activeTags}
              onToggle={toggleTag}
              onClear={() => setSelectedTags([])}
            />
          ) : undefined
        }
        filteredCount={filterActive ? visibleTasks.length : undefined}
      />

      {!collapsed && (
        <div
          ref={setNodeRef}
          className={`flex flex-col gap-2 overflow-y-auto rounded-chip border pr-0.5 transition-colors min-h-0 flex-1 ${
            showAffordance ? decisionClasses(decision, isOver) : "border-transparent"
          } ${showAffordance && decision !== "invalid" ? "p-1" : ""}`}
        >
          {visibleTasks.map((t) => {
            const isWorktreeColumn = lane.kind === "worktree";
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
                isWorktreeColumn={isWorktreeColumn}
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
              />
            );
          })}
          {visibleTasks.length === 0 && (
            <div className="rounded-chip border border-dashed border-line/60 px-2.5 py-3 text-center text-[11px] text-tx-3">
              {filterActive ? "no tasks match" : "—"}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

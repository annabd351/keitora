// TaskCard — extracted verbatim from Board.tsx's `Card` component (≈L112–362).
// Props, useDraggable wiring, reserved/ready/sent/terminal logic, claim/done/release
// buttons, and usePrefersReducedMotion gating are unchanged. Only imports are fixed up
// for the new location under src/components/board/.

import { useDraggable } from "@dnd-kit/core";
import { motion } from "motion/react";
import type { WorktreeSession } from "../../lib/sessions";
import { PRIORITY_BAR } from "../../lib/taskStyles";
import { claimTask, type TaskWithDeps } from "../../lib/tasks";
import { usePrefersReducedMotion } from "../../lib/usePrefersReducedMotion";
import { DispatchMenu } from "../DispatchMenu";

// Tags render on a single (non-wrapping) line; beyond this many we collapse the rest
// into a "+N more" chip so the card grows in width predictably instead of wrapping.
const MAX_VISIBLE_TAGS = 3;

export function TaskCard({
  t,
  column,
  isWorktreeColumn,
  actor,
  selected,
  worktreeNames,
  sessions,
  isFrontReserved,
  alreadyDispatched,
  flyIn,
  menuOpen,
  onMenuOpenChange,
  onSelect,
  onAction,
  onDispatch,
  onRelease,
  compact = false,
  fill = false,
}: {
  t: TaskWithDeps;
  column: string;
  isWorktreeColumn: boolean;
  actor: string | null;
  selected: boolean;
  worktreeNames: string[];
  sessions: Map<string, WorktreeSession>;
  /** This is the front reserved card AND its worktree is now idle → ready. */
  isFrontReserved: boolean;
  /** This reserved card has already been injected into its worktree's PTY. */
  alreadyDispatched: boolean;
  /** Worktree this card just dispatched to (inline "dispatched → @wt" line). */
  flyIn: string | null;
  menuOpen: boolean;
  onMenuOpenChange: (open: boolean) => void;
  onSelect: () => void;
  onAction: (fn: () => Promise<unknown>) => void;
  onDispatch: (worktree: string) => void;
  onRelease: () => void;
  /** Compact variant: tighter padding, smaller text, suited to ~w-[188px] cards in rows. */
  compact?: boolean;
  /** Stretch the card body to fill its parent's height (desktop worktree-row cards). */
  fill?: boolean;
}) {
  const task = t;
  const blocked_by = t.blocked_by;
  const terminal = task.status === "done" || task.status === "cancelled";
  const reducedMotion = usePrefersReducedMotion();

  // A reserved card = open + assigned to *this* worktree column.
  const reserved = isWorktreeColumn && task.status === "open" && task.assigned_to === column;
  // Ready-to-dispatch = the front reserved card whose worktree is now idle,
  // and has not yet been dispatched (injected into the PTY).
  const ready = reserved && isFrontReserved && !alreadyDispatched;

  // Whole-card drag on non-terminal cards. Terminal → no drag (cursor-default).
  const draggable = useDraggable({
    id: task.id,
    data: { task },
    disabled: terminal,
  });
  const { attributes, listeners, setNodeRef, isDragging } = draggable;
  // The lifted visual is the DragOverlay; the origin slot stays put (opacity-40),
  // so we deliberately do NOT apply `draggable.transform` here.

  // One 600ms pulse when a card becomes ready-to-dispatch (gated on reduced motion).
  const pulse = ready && !reducedMotion ? "animate-[pulse_600ms_ease-in-out_1]" : "";

  // A reserved card is "emphasized" when it's the live dispatch target (ready) or
  // already sent — both get the brighter accent border/rail; plain queued cards stay dim.
  const emphasized = ready || (reserved && alreadyDispatched);

  // Card chrome: reserved swaps the solid bar for a dashed accent rail + border.
  const borderCls = emphasized
    ? "border-accent/60"
    : reserved
      ? "border-dashed border-accent/30"
      : selected
        ? "border-accent/55 shadow-[0_0_0_1px_color-mix(in_srgb,var(--color-accent)_25%,transparent)]"
        : "border-line";

  // Compact variant sizing tokens.
  const padCls = compact ? "px-2 py-2.5" : "px-2.5 py-2.5";
  const railInsetCls = "top-2.5 bottom-2.5";
  const idTextCls = compact ? "text-[10px]" : "text-[11px]";
  const titleTextCls = compact
    ? `mt-1 text-[11px] leading-snug line-clamp-2 ${reserved ? "text-tx-2" : "text-tx-1"}`
    : `mt-1.5 max-w-[200px] text-[12.5px] leading-snug ${reserved ? "text-tx-2" : "text-tx-1"}`;
  const badgeTextCls = compact ? "text-[9px]" : "text-[10px]";
  const tagTextCls = compact ? "text-[9px]" : "text-[10px]";
  const actionMtCls = compact ? "mt-1.5" : "mt-2.5";

  const cardBody = (
    <div
      ref={setNodeRef}
      onClick={onSelect}
      {...(terminal ? {} : listeners)}
      {...attributes}
      className={`group relative rounded-chip border bg-bg-2 ${padCls} ${fill ? "flex h-full flex-col" : ""} ${
        terminal ? "cursor-default" : "cursor-pointer"
      } ${borderCls} ${terminal ? "opacity-60" : ""} ${isDragging ? "opacity-40" : ""} ${pulse}`}
    >
      {/* Left rail: dashed accent for reserved, solid priority bar otherwise. */}
      {reserved ? (
        <span
          className={`absolute left-0 ${railInsetCls} w-0 border-l-2 border-dashed ${
            emphasized ? "border-accent/70" : "border-accent/50"
          }`}
        />
      ) : (
        <span
          className={`absolute left-0 ${railInsetCls} w-[3px] rounded ${PRIORITY_BAR[task.priority]}`}
        />
      )}

      <div className="flex items-center gap-2 pl-2">
        <span className={`font-mono ${idTextCls} text-tx-3`}>#{task.id}</span>
        {/* Reserved cards preserve priority as a small dot next to the id. */}
        {reserved && (
          <span
            className={`h-1.5 w-1.5 flex-none rounded-full ${PRIORITY_BAR[task.priority]}`}
            title={task.priority}
          />
        )}

        {!terminal && (
          <DispatchMenu
            card={task}
            worktreeNames={worktreeNames}
            sessions={sessions}
            open={menuOpen}
            onOpenChange={onMenuOpenChange}
            onDispatch={onDispatch}
            onRelease={onRelease}
          />
        )}

        {/* Right-side badge slot: queued chip / dispatch button. */}
        {ready && (
          <button
            onClick={(e) => {
              e.stopPropagation();
              onDispatch(column);
            }}
            onPointerDown={(e) => e.stopPropagation()}
            className={`ml-auto rounded-chip border border-accent bg-accent px-2 py-px ${badgeTextCls} font-semibold text-[var(--color-on-accent)]`}
          >
            Dispatch ↵
          </button>
        )}
        {reserved && alreadyDispatched && (
          <span
            className={`ml-auto rounded-full border border-accent/40 bg-accent-dim px-2 py-px ${badgeTextCls} text-accent-text`}
          >
            sent ✓
          </span>
        )}
        {reserved && !ready && !alreadyDispatched && (
          <span
            className={`ml-auto rounded-full border border-accent/40 bg-accent-dim px-2 py-px ${badgeTextCls} text-accent-text`}
          >
            queued
          </span>
        )}
        {terminal && <span className="ml-auto text-idle">✓</span>}

        {/* Blocked indicator: top-right of the header, in line with the #id. Pushed
            right with ml-auto; if a reserved badge is also present they sit adjacent. */}
        {blocked_by.length > 0 && (
          <span
            className={`ml-auto whitespace-nowrap rounded-full bg-urgent/10 px-2 py-px ${badgeTextCls} text-urgent`}
          >
            blocked by {blocked_by.map((b) => `#${b}`).join(", ")}
          </span>
        )}
      </div>

      <div className={`pl-2 ${titleTextCls}`}>{task.title}</div>

      {/* Inline dispatch fly-in line (until the worktree flips it to active). */}
      {flyIn && task.status === "open" && (
        <div className={`${compact ? "mt-1" : "mt-1.5"} pl-2 text-[10.5px] text-accent-text`}>
          dispatched → @{flyIn}
        </div>
      )}

      {task.tags.length > 0 && (
        <div
          className={`${fill ? "mt-auto pt-1.5" : compact ? "mt-1.5" : "mt-2"} flex flex-nowrap items-center gap-1.5 pl-2`}
        >
          {task.tags.slice(0, MAX_VISIBLE_TAGS).map((tag) => (
            <span
              key={tag}
              className={`whitespace-nowrap rounded-full border border-line bg-bg-3 px-2 py-px ${tagTextCls} text-tx-2`}
            >
              {tag}
            </span>
          ))}
          {task.tags.length > MAX_VISIBLE_TAGS && (
            <span
              className={`whitespace-nowrap rounded-full border border-line bg-bg-3 px-2 py-px ${tagTextCls} text-tx-3`}
            >
              +{task.tags.length - MAX_VISIBLE_TAGS} more
            </span>
          )}
        </div>
      )}
      {/* Reserved cards hide Claim (a human must not self-claim an agent's reserved
          task); they expose a subtle hover Release instead. */}
      {reserved ? (
        <div
          className={`${actionMtCls} pl-2 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100`}
        >
          <button
            onClick={(e) => {
              e.stopPropagation();
              onRelease();
            }}
            onPointerDown={(e) => e.stopPropagation()}
            className="rounded-chip border border-line bg-bg-3 px-2.5 py-1 text-[11px] text-tx-1"
          >
            Release
          </button>
        </div>
      ) : (
        task.status === "open" &&
        blocked_by.length === 0 &&
        actor !== null && (
          <div className={`${actionMtCls} pl-2`}>
            <button
              onClick={(e) => {
                e.stopPropagation();
                onAction(() => claimTask(task.id, actor));
              }}
              onPointerDown={(e) => e.stopPropagation()}
              className="rounded-chip border border-accent bg-accent px-2.5 py-1 text-[11px] font-semibold text-[var(--color-on-accent)]"
            >
              Claim
            </button>
          </div>
        )
      )}
    </div>
  );

  // A just-dispatched card mounts with a one-shot opacity fade (transforms-safe);
  // reduced motion = appears in place.
  if (flyIn && !reducedMotion) {
    return (
      <motion.div
        className={fill ? "h-full" : undefined}
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ duration: 0.22 }}
      >
        {cardBody}
      </motion.div>
    );
  }
  return cardBody;
}

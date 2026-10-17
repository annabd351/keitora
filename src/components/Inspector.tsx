// Right-hand contextual panel: full detail for the selected task — status, tags,
// dependency tree, description, activity log, and contextual actions. Reloads on
// task selection and whenever the board's task list changes (revision prop).
// Gains `sessions` + `proposals` props for the dispatch proposal panel.

import { ChevronLeft, ChevronRight } from "lucide-react";
import { useEffect, useState } from "react";
import type { Proposal } from "../lib/dispatch";
import { dispatchApply } from "../lib/dispatch";
import type { WorktreeSession } from "../lib/sessions";
import { PRIORITY_PILL, STATUS_DOT, sessionDotCls, shortTime } from "../lib/taskStyles";
import {
  claimTask,
  deleteTask,
  doneTask,
  errorMessage,
  getTask,
  type TaskDetail,
  unclaimTask,
  updateTask,
} from "../lib/tasks";
import { ConfidencePip } from "./ConfidencePip";
import { TaskFormDialog } from "./TaskFormDialog";
import { useToast } from "./Toast";

export function Inspector({
  taskId,
  revision,
  actor,
  reload,
  onClose,
  sessions,
  proposals,
  onProposalSpent,
  collapsed,
  onToggleCollapsed,
}: {
  taskId: number | null;
  /** Bumps when the board reloads, so the open task refetches in step. */
  revision: number;
  actor: string | null;
  reload: () => Promise<void>;
  /** Clear the parent's task selection (e.g. after a successful delete). */
  onClose: () => void;
  /** Live worktree session states for the proposal panel. */
  sessions: Map<string, WorktreeSession>;
  /** Active proposals from the dispatcher, keyed by task_id. */
  proposals: Map<number, Proposal>;
  /** Called when the user approves a proposal (removes it from the map). */
  onProposalSpent: (taskId: number) => void;
  /** When true, the panel folds into a skinny rail with an expand affordance. */
  collapsed: boolean;
  /** Toggles the collapsed state (persisted by the parent). */
  onToggleCollapsed: () => void;
}) {
  const [detail, setDetail] = useState<TaskDetail | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [editOpen, setEditOpen] = useState(false);
  // Two-click delete confirm; reset whenever the selected task changes so
  // switching tasks never leaves a primed confirm.
  const [confirmDelete, setConfirmDelete] = useState(false);
  const { showToast } = useToast();

  // biome-ignore lint/correctness/useExhaustiveDependencies: taskId is the intended trigger — reset the primed confirm when the selected task changes; removing it would break the reset.
  useEffect(() => {
    setConfirmDelete(false);
  }, [taskId]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: revision triggers refetch but is an outer-scope ref value; biome flags it as invalid dep
  useEffect(() => {
    if (taskId === null) {
      setDetail(null);
      setErr(null);
      return;
    }
    let cancelled = false;
    // Clear any error from the previously-selected task so it isn't shown while
    // the new task loads.
    setErr(null);
    void getTask(taskId)
      .then((d) => {
        if (!cancelled) {
          setDetail(d);
          setErr(null);
        }
      })
      .catch((e) => !cancelled && setErr(errorMessage(e)));
    return () => {
      cancelled = true;
    };
  }, [taskId, revision]);

  // Collapsed: a skinny rail. The chevron points left — the direction the panel
  // travels when reopened. Wins over the empty/full branches so the panel stays
  // collapsible regardless of selection.
  if (collapsed) {
    return (
      <aside
        id="inspector-panel"
        className="flex w-9 flex-none flex-col items-center border-l border-line bg-bg-1 transition-[width] duration-200 ease-out"
      >
        <CollapseToggle
          collapsed={collapsed}
          onToggle={onToggleCollapsed}
          className="flex h-11 w-full items-center justify-center text-tx-3 transition-colors hover:text-tx-1"
        />
        <span className="mt-1 select-none text-[10px] uppercase tracking-wider text-tx-3 [writing-mode:vertical-rl]">
          Inspector
        </span>
      </aside>
    );
  }

  const act = (fn: () => Promise<unknown>) => {
    setErr(null);
    void fn()
      .then(() => reload())
      .catch((e) => setErr(errorMessage(e)));
  };

  if (taskId === null || detail === null) {
    return (
      <aside
        id="inspector-panel"
        className="flex w-[312px] flex-none overflow-hidden border-l border-line bg-bg-1 transition-[width] duration-200 ease-out"
      >
        <div className="flex h-full w-[312px] flex-none flex-col">
          <div className="flex items-center border-b border-line px-3.5 py-3">
            <CollapseToggle collapsed={collapsed} onToggle={onToggleCollapsed} />
          </div>
          <div className="m-auto px-6 text-center text-[12px] text-tx-3">
            {err ?? "Select a task to inspect"}
          </div>
        </div>
      </aside>
    );
  }

  const task = detail.task;
  const blocked_by = task.blocked_by;
  const blocking = new Set(blocked_by);
  const mine = task.status === "active" && task.assigned_to === actor;

  // Delete: on success clear the selection (the task is gone); on a Conflict
  // (dependents exist) keep the task selected so the inline error is visible.
  const onDelete = () => {
    setErr(null);
    deleteTask(task.id)
      .then(() => {
        setConfirmDelete(false);
        void reload();
        onClose();
      })
      .catch((e) => {
        setErr(errorMessage(e));
        setConfirmDelete(false);
      });
  };

  // Proposal panel — shown when this task has a live proposal.
  const proposal = proposals.get(task.id) ?? null;

  return (
    <aside
      id="inspector-panel"
      className="flex w-[312px] flex-none overflow-hidden border-l border-line bg-bg-1 transition-[width] duration-200 ease-out"
    >
      {/* Fixed-width inner pane: content lays out at the full width regardless of
          the aside's animating width, so text wrapping doesn't reflow mid-fold. */}
      <div className="flex h-full w-[312px] flex-none flex-col">
        <div className="flex items-center gap-2.5 border-b border-line px-3.5 py-3">
          <CollapseToggle collapsed={collapsed} onToggle={onToggleCollapsed} />
          <span className="font-mono text-[12px] text-tx-3">#{task.id}</span>
          <span
            className={`ml-auto rounded-full px-2.5 py-0.5 text-[10px] capitalize ${PRIORITY_PILL[task.priority]}`}
          >
            {task.priority}
          </span>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto p-3.5">
          {/* ── Proposal panel (rendered ABOVE task detail when present) ── */}
          {proposal && (
            <div className="mb-4">
              <ProposalPanel
                proposal={proposal}
                sessions={sessions}
                onProposalSpent={onProposalSpent}
                reload={reload}
                showToast={showToast}
              />
            </div>
          )}

          {/* ── Standard task detail ── */}
          <h2 className="mb-3 text-[15px] font-semibold leading-snug text-tx-1">{task.title}</h2>

          <Field label="Status">
            <span className="inline-flex items-center gap-1.5">
              <span className={`inline-block h-2 w-2 rounded-full ${STATUS_DOT[task.status]}`} />
              {task.status} · {task.assigned_to ? `@${task.assigned_to}` : "unassigned"}
            </span>
          </Field>

          {task.tags.length > 0 && (
            <Field label="Tags">
              <span className="flex flex-wrap gap-1.5">
                {task.tags.map((t) => (
                  <span
                    key={t}
                    className="rounded-full border border-line bg-bg-3 px-2 py-px text-[10px] text-tx-2"
                  >
                    {t}
                  </span>
                ))}
              </span>
            </Field>
          )}

          {detail.deps.length > 0 && (
            <Field label="Dependencies">
              <div className="flex flex-col">
                {detail.deps.map((d) => {
                  const done = d.status === "done";
                  const stillBlocking = blocking.has(d.id);
                  return (
                    <div key={d.id} className="flex items-center gap-2 py-1 text-[12px] text-tx-2">
                      <span
                        className={done ? "text-idle" : stillBlocking ? "text-urgent" : "text-tx-3"}
                      >
                        {done ? "✓" : "•"}
                      </span>
                      <span className="font-mono text-tx-3">#{d.id}</span>
                      <span className="truncate">{d.title}</span>
                      <span className="ml-auto text-tx-3">({d.status})</span>
                    </div>
                  );
                })}
              </div>
            </Field>
          )}

          {task.branch && (
            <Field label="Branch">
              <span className="font-mono text-[12px] text-tx-2">{task.branch}</span>
            </Field>
          )}

          {task.description && (
            <Field label="Description">
              <p className="whitespace-pre-wrap text-[12px] leading-relaxed text-tx-2">
                {task.description}
              </p>
            </Field>
          )}

          {detail.logs.length > 0 && (
            <div className="mt-1.5 border-t border-line pt-3">
              <div className="mb-2 text-[10.5px] uppercase tracking-wide text-tx-3">Activity</div>
              {detail.logs.map((l, i) => (
                // biome-ignore lint/suspicious/noArrayIndexKey: append-only activity log; no stable unique id on log entries
                <div key={i} className="flex gap-2.5 py-1 text-[11px] text-tx-2">
                  <span className="flex-none font-mono text-tx-3">{shortTime(l.created_at)}</span>
                  <span className="flex-none text-accent-text">{l.action}</span>
                  <span className="truncate">{l.detail || `@${l.worktree}`}</span>
                </div>
              ))}
            </div>
          )}

          {err && <p className="mt-3 text-[11px] text-urgent">{err}</p>}
        </div>

        <div className="flex flex-wrap gap-2 border-t border-line px-3.5 py-3">
          {task.status === "open" && blocked_by.length === 0 && actor !== null && (
            <button
              onClick={() => act(() => claimTask(task.id, actor))}
              className="flex-1 rounded-chip border border-accent bg-accent py-1.5 text-center text-[11.5px] font-semibold text-[var(--color-on-accent)]"
            >
              Claim
            </button>
          )}
          {mine && actor !== null && (
            <>
              <button
                onClick={() => act(() => doneTask(task.id, actor))}
                className="flex-1 rounded-chip border border-accent bg-accent py-1.5 text-center text-[11.5px] font-semibold text-[var(--color-on-accent)]"
              >
                Done
              </button>
              <button
                onClick={() => act(() => unclaimTask(task.id, actor))}
                className="flex-1 rounded-chip border border-line bg-bg-3 py-1.5 text-center text-[11.5px] text-tx-1"
              >
                Unclaim
              </button>
            </>
          )}
          <button
            onClick={() => setEditOpen(true)}
            className="flex-1 rounded-chip border border-line bg-bg-3 py-1.5 text-center text-[11.5px] text-tx-1"
          >
            Edit
          </button>
          {confirmDelete ? (
            <>
              <button
                onClick={onDelete}
                className="flex-1 whitespace-nowrap rounded-chip border border-urgent bg-urgent py-1.5 text-center text-[11.5px] font-semibold text-white"
              >
                Confirm delete?
              </button>
              <button
                onClick={() => setConfirmDelete(false)}
                className="flex-1 whitespace-nowrap rounded-chip border border-line bg-bg-3 py-1.5 text-center text-[11.5px] text-tx-1"
              >
                Cancel
              </button>
            </>
          ) : (
            <button
              onClick={() => setConfirmDelete(true)}
              className="flex-1 rounded-chip border border-line bg-bg-3 py-1.5 text-center text-[11.5px] text-urgent"
            >
              Delete
            </button>
          )}
        </div>

        <TaskFormDialog
          open={editOpen}
          onOpenChange={setEditOpen}
          mode="edit"
          initial={{ title: task.title, priority: task.priority, description: task.description }}
          onSubmit={async (v) => {
            await updateTask(task.id, {
              title: v.title,
              priority: v.priority,
              description: v.description,
            });
            await reload();
          }}
        />
      </div>
    </aside>
  );
}

// Single source of truth for the collapse/expand affordance. Label, icon, and
// aria-expanded all derive from `collapsed`; aria-controls ties it to the panel.
function CollapseToggle({
  collapsed,
  onToggle,
  className = "-ml-1 flex h-5 w-5 items-center justify-center text-tx-3 transition-colors hover:text-tx-1",
}: {
  collapsed: boolean;
  onToggle: () => void;
  className?: string;
}) {
  return (
    <button
      onClick={onToggle}
      aria-label={collapsed ? "Expand inspector" : "Collapse inspector"}
      aria-expanded={!collapsed}
      aria-controls="inspector-panel"
      className={className}
    >
      {collapsed ? <ChevronLeft size={16} /> : <ChevronRight size={16} />}
    </button>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="mb-3">
      <div className="mb-1.5 text-[10.5px] uppercase tracking-wide text-tx-3">{label}</div>
      <div className="text-[12.5px] text-tx-1">{children}</div>
    </div>
  );
}

// Dispatch-proposal Approve/Reserve panel — extracted from the inline Inspector
// JSX. Pure presentation + the approve/queue mutation; no layout chrome
// of its own beyond the bordered card (callers position it).
function ProposalPanel({
  proposal,
  sessions,
  onProposalSpent,
  reload,
  showToast,
}: {
  proposal: Proposal;
  sessions: Map<string, WorktreeSession>;
  onProposalSpent: (taskId: number) => void;
  reload: () => Promise<void>;
  showToast: (msg: string) => void;
}) {
  const [approving, setApproving] = useState(false);

  const targetSession = sessions.get(proposal.worktree) ?? null;
  const targetState = targetSession?.state ?? "offline";
  const canApprove = targetState === "idle";
  // Busy/waiting → Approve is disabled, but a deliberate Reserve (queue) is
  // offered; the backend turns the same dispatchApply into a queue. Offline
  // stays a dead-end with a Re-suggest hint.
  const canReserve = targetState === "busy" || targetState === "waiting";

  const approveReason: string | null =
    targetState === "busy"
      ? `@${proposal.worktree} is busy — Reserve instead?`
      : targetState === "waiting"
        ? `@${proposal.worktree} is waiting — Reserve instead?`
        : targetState === "offline"
          ? `@${proposal.worktree} went offline — Re-suggest`
          : null;

  const handleApprove = async () => {
    setApproving(true);
    try {
      const outcome = await dispatchApply(proposal.worktree, proposal.task_id);
      if (!outcome.dispatched) {
        showToast(`Queued #${proposal.task_id} — @${proposal.worktree} busy`);
      }
      onProposalSpent(proposal.task_id);
      await reload();
    } catch (e) {
      showToast(errorMessage(e));
    } finally {
      setApproving(false);
    }
  };

  return (
    <div className="rounded-card border border-accent/30 bg-accent-dim p-3">
      <div className="mb-2 flex items-center gap-2">
        <span className="text-[10.5px] uppercase tracking-wide text-accent-text">
          ⚡ Dispatch proposal
        </span>
      </div>

      {/* Target worktree with live session dot */}
      <div className="mb-2 flex items-center gap-1.5">
        <span className="text-[12px] text-tx-2">→</span>
        <span className={`h-1.5 w-1.5 rounded-full ${sessionDotCls(targetState)}`} />
        <span className="font-mono text-[12px] text-tx-1">@{proposal.worktree}</span>
        <span className="ml-auto">
          <ConfidencePip confidence={proposal.confidence} showPercent />
        </span>
      </div>

      {/* Full rationale */}
      <p className="mb-3 text-[11.5px] leading-relaxed text-tx-2">{proposal.rationale}</p>

      {/* Approve button — gated by idle state */}
      <button
        type="button"
        onClick={() => void handleApprove()}
        disabled={!canApprove || approving}
        className="w-full rounded-chip border border-accent bg-accent py-1.5 text-center text-[11.5px] font-semibold text-[var(--color-on-accent)] disabled:cursor-not-allowed disabled:opacity-50"
        title={approveReason ?? undefined}
      >
        {approving ? "Dispatching…" : `Approve → dispatch to @${proposal.worktree}`}
      </button>

      {/* Busy/waiting → a deliberate Reserve (queue) fallback. Dashed accent
          = "will defer", per the accent discipline. */}
      {canReserve && (
        <button
          type="button"
          onClick={() => void handleApprove()}
          disabled={approving}
          className="mt-1.5 w-full rounded-chip border border-dashed border-accent/50 bg-accent-dim py-1.5 text-center text-[11.5px] font-semibold text-accent-text disabled:cursor-not-allowed disabled:opacity-50"
        >
          {approving ? "Reserving…" : `Reserve → queue on @${proposal.worktree}`}
        </button>
      )}

      {/* Honest reason when not idle */}
      {approveReason && (
        <p className="mt-1.5 text-center text-[10.5px] text-tx-3">{approveReason}</p>
      )}
    </div>
  );
}

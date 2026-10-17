// Dispatch strip atop the Board: a Suggest control + a horizontally-scrollable
// row of ProposalCards. Collapses to zero height when there are no proposals and
// not loading. No drag wiring.

import * as Tooltip from "@radix-ui/react-tooltip";
import { Loader2, Zap } from "lucide-react";
import type { Proposal } from "../lib/dispatch";
import { modKeyLabel } from "../lib/platform";
import type { WorktreeSession } from "../lib/sessions";
import type { TaskWithDeps } from "../lib/tasks";
import { ProposalCard } from "./ProposalCard";

interface DispatchStripProps {
  proposals: Map<number, Proposal>;
  loading: boolean;
  error: string | null;
  /** How many worktrees are currently idle. Controls the disabled state. */
  idleCount: number;
  onSuggest: () => void;
  /** Currently selected task id (for ProposalCard ring). */
  selectedId: number | null;
  onSelect: (taskId: number) => void;
  /** Full task list for looking up titles. */
  tasks: TaskWithDeps[];
  sessions: Map<string, WorktreeSession>;
}

export function DispatchStrip({
  proposals,
  loading,
  error,
  idleCount,
  onSuggest,
  selectedId,
  onSelect,
  tasks,
  sessions,
}: DispatchStripProps) {
  const proposalList = Array.from(proposals.values());
  const hasProposals = proposalList.length > 0;
  const noIdle = idleCount === 0;

  // The Suggest control row is ALWAYS visible (it's the feature's primary entry
  // point; ⌘K is the shortcut, not the only path). Only the proposal-card row
  // below collapses when there's nothing to show.

  // Build a title lookup map.
  const titleById = new Map(tasks.map((t) => [t.id, t.title]));

  return (
    <div className="flex flex-col border-b border-line bg-bg-0">
      {/* Indeterminate loading bar */}
      {loading && (
        <div className="h-px w-full overflow-hidden">
          <div className="h-px animate-[indeterminate_1.5s_ease-in-out_infinite] bg-accent/40" />
        </div>
      )}

      <div className="flex items-center gap-3 px-3.5 py-2">
        {/* Suggest button */}
        <Tooltip.Root delayDuration={500}>
          <Tooltip.Trigger asChild>
            <button
              type="button"
              onClick={onSuggest}
              disabled={loading || noIdle}
              className="flex flex-none items-center gap-1.5 rounded-chip border border-accent/40 bg-accent-dim px-2.5 py-1 text-[11.5px] text-accent-text disabled:cursor-not-allowed disabled:opacity-50"
            >
              {loading ? <Loader2 size={12} className="animate-spin" /> : <Zap size={12} />}
              {loading ? "Thinking…" : "Suggest"}
              {!loading && !noIdle && (
                <span className="ml-1 text-[10px] text-tx-3">{modKeyLabel}K</span>
              )}
            </button>
          </Tooltip.Trigger>
          {noIdle && (
            <Tooltip.Portal>
              <Tooltip.Content
                className="rounded-chip border border-line bg-bg-2 px-2 py-1 text-[11px] text-tx-2 shadow-md"
                sideOffset={4}
              >
                No idle worktrees to dispatch to
                <Tooltip.Arrow className="fill-bg-2" />
              </Tooltip.Content>
            </Tooltip.Portal>
          )}
        </Tooltip.Root>

        {/* No idle worktrees inline note. Complements the tooltip (only when there
            are no proposals). */}
        {noIdle && !loading && !error && !hasProposals && (
          <span className="text-[11px] text-tx-3">No idle worktrees to dispatch to</span>
        )}

        {/* Error state */}
        {error && !loading && (
          <span className="flex items-center gap-2">
            <span className="truncate text-[11px] text-urgent">{error}</span>
            <button
              type="button"
              onClick={onSuggest}
              className="flex-none rounded-chip border border-line bg-bg-3 px-2 py-0.5 text-[10.5px] text-tx-1 hover:bg-bg-2"
            >
              Retry
            </button>
          </span>
        )}

        {/* Empty result note (no error, no proposals, not loading, had idle) */}
        {!loading && !error && !hasProposals && !noIdle && (
          <span className="text-[11px] text-tx-3">No good matches right now</span>
        )}
      </div>

      {/* Proposals: a horizontally-scrollable row. */}
      {hasProposals && (
        <div className="flex gap-2 overflow-x-auto px-3.5 pb-2.5 pt-0">
          {proposalList.map((p) => {
            const session = sessions.get(p.worktree);
            const state = session?.state ?? "offline";
            const title = titleById.get(p.task_id) ?? `Task #${p.task_id}`;
            return (
              <ProposalCard
                key={`${p.worktree}:${p.task_id}`}
                proposal={p}
                taskTitle={title}
                sessionState={state}
                selected={p.task_id === selectedId}
                onSelect={onSelect}
              />
            );
          })}
        </div>
      )}
    </div>
  );
}

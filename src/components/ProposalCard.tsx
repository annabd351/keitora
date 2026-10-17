// A single dispatch proposal card for the DispatchStrip. Shows worktree →
// task-id, truncated title, ConfidencePip, and truncated rationale. The whole
// card is clickable to open the selected task in the Inspector.

import type { Proposal } from "../lib/dispatch";
import type { SessionState } from "../lib/sessions";
import { sessionDotCls } from "../lib/taskStyles";
import { ConfidencePip } from "./ConfidencePip";

interface ProposalCardProps {
  proposal: Proposal;
  taskTitle: string;
  /** Live session state of the proposal's target worktree. */
  sessionState: SessionState;
  selected: boolean;
  onSelect: (taskId: number) => void;
}

export function ProposalCard({
  proposal,
  taskTitle,
  sessionState,
  selected,
  onSelect,
}: ProposalCardProps) {
  return (
    <button
      type="button"
      onClick={() => onSelect(proposal.task_id)}
      className={`flex w-[200px] flex-none flex-col gap-1.5 rounded-card border bg-bg-2 px-3 py-2.5 text-left transition-colors hover:bg-bg-3 ${
        selected
          ? "border-accent/55 shadow-[0_0_0_1px_color-mix(in_srgb,var(--color-accent)_25%,transparent)]"
          : "border-line"
      }`}
    >
      {/* Top line: dot + worktree → #id */}
      <div className="flex items-center gap-1.5">
        <span className={`h-1.5 w-1.5 flex-none rounded-full ${sessionDotCls(sessionState)}`} />
        <span className="truncate text-[11.5px] font-medium text-tx-1">{proposal.worktree}</span>
        <span className="ml-auto flex-none font-mono text-[10.5px] text-tx-3">
          #{proposal.task_id}
        </span>
      </div>

      {/* Task title */}
      <div className="truncate text-[12px] text-tx-2">{taskTitle}</div>

      {/* Confidence pip (tooltip shows percent on board surface) */}
      <ConfidencePip confidence={proposal.confidence} />

      {/* Rationale */}
      <div className="truncate text-[11px] text-tx-3">{proposal.rationale}</div>
    </button>
  );
}

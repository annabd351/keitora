// Per-card "Dispatch to…" menu — the keyboard-first parity path for drag-to-
// dispatch (proposal §4.3). A Radix DropdownMenu opened by a caret on the card
// header (hover/focus-visible) or by `D` when the card is selected (the parent
// drives `open`). Groups, in order: idle worktrees (→ dispatch, accent), busy/
// waiting (→ queue, dim), offline (disabled), then a Release item if assigned.

import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { ChevronDown } from "lucide-react";
import type { SessionState, WorktreeSession } from "../lib/sessions";
import { STATE_ORDER, sessionDotCls } from "../lib/taskStyles";
import type { TaskWithDeps } from "../lib/tasks";

export function DispatchMenu({
  card,
  worktreeNames,
  sessions,
  open,
  onOpenChange,
  onDispatch,
  onRelease,
}: {
  card: TaskWithDeps;
  worktreeNames: string[];
  sessions: Map<string, WorktreeSession>;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onDispatch: (worktree: string) => void;
  onRelease: () => void;
}) {
  // Candidate worktrees = all lanes except the one the card already sits in.
  // Sort idle → busy → waiting → offline so the actionable targets lead.
  const targets = worktreeNames
    .filter((wt) => wt !== card.assigned_to)
    .map((wt) => ({ wt, state: (sessions.get(wt)?.state ?? "offline") as SessionState }))
    .sort((a, b) => STATE_ORDER[a.state] - STATE_ORDER[b.state]);

  const blocked = card.blocked_by.length > 0;

  const itemCls =
    "flex cursor-pointer items-center gap-2 rounded-chip px-2 py-1.5 text-[11.5px] outline-none data-[highlighted]:bg-bg-3 data-[disabled]:cursor-not-allowed data-[disabled]:opacity-45";

  return (
    <DropdownMenu.Root open={open} onOpenChange={onOpenChange}>
      <DropdownMenu.Trigger asChild>
        <button
          type="button"
          aria-label="Dispatch to…"
          onClick={(e) => e.stopPropagation()}
          onPointerDown={(e) => e.stopPropagation()}
          className="ml-1 flex-none rounded text-tx-3 opacity-0 outline-none transition-opacity hover:text-tx-1 focus-visible:opacity-100 group-hover:opacity-100 data-[state=open]:opacity-100"
        >
          <ChevronDown size={13} />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={4}
          onClick={(e) => e.stopPropagation()}
          className="z-50 min-w-[180px] rounded-card border border-line bg-bg-2 p-1 shadow-[0_8px_24px_rgba(0,0,0,.45)]"
        >
          <DropdownMenu.Label className="px-2 py-1 text-[10px] uppercase tracking-wide text-tx-3">
            Dispatch to…
          </DropdownMenu.Label>

          {blocked && (
            <div className="px-2 py-1.5 text-[11px] text-urgent">Blocked — resolve deps first</div>
          )}

          {!blocked &&
            targets.map((tgt, i) => {
              const prev = targets[i - 1]?.state;
              // Separator between the action tiers (idle | busy/waiting | offline).
              const tier = (s: SessionState) => (s === "idle" ? 0 : s === "offline" ? 2 : 1);
              const sep = i > 0 && prev !== undefined && tier(prev) !== tier(tgt.state);
              const isIdle = tgt.state === "idle";
              const isOffline = tgt.state === "offline";
              const verb = isIdle ? "→ dispatch" : isOffline ? "offline" : "→ queue";
              return (
                <div key={tgt.wt}>
                  {sep && <DropdownMenu.Separator className="my-1 h-px bg-line" />}
                  <DropdownMenu.Item
                    disabled={isOffline}
                    onSelect={() => onDispatch(tgt.wt)}
                    className={`${itemCls} ${isIdle ? "text-accent-text" : "text-tx-2"}`}
                  >
                    <span
                      className={`h-1.5 w-1.5 flex-none rounded-full ${sessionDotCls(tgt.state)}`}
                    />
                    <span className="font-mono">@{tgt.wt}</span>
                    <span className="ml-auto text-[10px] text-tx-3">{verb}</span>
                  </DropdownMenu.Item>
                </div>
              );
            })}

          {card.assigned_to && (
            <>
              <DropdownMenu.Separator className="my-1 h-px bg-line" />
              <DropdownMenu.Item onSelect={() => onRelease()} className={`${itemCls} text-tx-2`}>
                <span className="h-1.5 w-1.5 flex-none rounded-full bg-tx-3" />
                <span>Release to Open</span>
              </DropdownMenu.Item>
            </>
          )}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

// Pure helpers for rendering worktree activity on the rail. Kept framework-free so they
// are unit-tested without the DOM and reused by the lane components. They read only the
// camelCase shapes normalized in `./ipc` — there are no process/agent counts here.

import type { WorkflowState, WorktreeActivity } from "./ipc";

/**
 * Compact chip text for a worktree's workflow stage. Underscores are prettified to spaces
 * (`review_gate` → `review gate`):
 *   - `complete`                   → `"complete"` (terminal — no fraction; "complete 18/19"
 *                                     reads as a contradiction)
 *   - `review_gate` with a PR url  → `"review gate · PR #126"`
 *   - stage with known progress    → `"implementing · 4/7"`
 *   - otherwise                    → the stage name
 */
export function stageChipText(w: WorkflowState): string {
  const label = w.stage.replace(/_/g, " ");
  // A finished workflow is just done; appending a stage fraction only confuses.
  if (w.stage === "complete") return label;
  if (w.stage === "review_gate" && w.prUrl) {
    const m = w.prUrl.match(/\/pull\/(\d+)/);
    if (m) return `${label} · PR #${m[1]}`;
  }
  if (w.done != null && w.total != null && w.total > 0) {
    return `${label} · ${w.done}/${w.total}`;
  }
  return label;
}

/** CSS tone for the stage chip — maps to `.lane-stage.<tone>` (default has no modifier). */
export type StageTone = "default" | "warn" | "stale" | "gate" | "done" | "none";

/**
 * Colour tone for the stage chip: `review_gate` → gate (needs you, urgent); `complete` → done
 * (a calm, finished outline); a stale stage → stale (muted); an unreadable/branch-mismatched
 * state → warn (amber, data is suspect); no workflow → none (the chip isn't rendered); else
 * the default (a subtle accent tint for an in-progress stage). `review_gate` and `complete`
 * are terminal/attention states, so they win over stale. Stale is a display hint only — it
 * never means the worktree is idle.
 */
export function stageChipTone(w: WorkflowState | null | undefined): StageTone {
  if (!w) return "none";
  if (w.stage === "review_gate") return "gate";
  if (w.stage === "complete") return "done";
  if (w.stale) return "stale";
  if (w.unreadable || w.branchMismatch) return "warn";
  return "default";
}

/** Humanize an ETA in seconds: `~45s`, `~4m`, `~2h 5m`. `null` (hidden/unknown) → `null`. */
export function formatEta(secs: number | null): string | null {
  if (secs == null || !Number.isFinite(secs) || secs < 0) return null;
  if (secs < 60) return `~${Math.round(secs)}s`;
  if (secs < 3600) return `~${Math.round(secs / 60)}m`;
  const h = Math.floor(secs / 3600);
  const m = Math.round((secs % 3600) / 60);
  return `~${h}h ${m}m`;
}

/** `"12/340 actions"`, or `"—/— actions"` when the total is unknown. */
export function formatActions(done: number, total: number): string {
  if (total > 0) return `${done}/${total} actions`;
  return "—/— actions";
}

/**
 * Whether a lane shows its build bar. Row 3 is simply "a build bar when a build payload is
 * present, else nothing" — the backend already decides whether a (running/finished-but-recent)
 * build is worth surfacing, so the lane just renders whatever `build` it is handed.
 */
export function showBuildBar(act: WorktreeActivity | null | undefined): boolean {
  return act?.build != null;
}

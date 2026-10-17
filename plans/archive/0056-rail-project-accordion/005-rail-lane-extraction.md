# Stage 5 — Extract Lane from Rail

## Goal

Move the lane row and its helpers out of `Rail.tsx` into `src/components/rail/Lane.tsx`
**verbatim, with no visual or behavioural change**, so the later focus treatment is a reviewable
diff on its own. Proposal section: §3 and the Risks entry on the Rail refactor.

## Prerequisites

- Stage 3 committed (`laneState.ts` already holds `blockStateCls`, `blockAnimates`, `worktreeMenuItems`).

## Implementation

`src/components/rail/Lane.tsx` (new)
- Today's `worktrees.map` body (`Rail.tsx:613-731`) becomes a `Lane` component driven by a
  `LaneModel` (from `src/lib/railGroups.ts`). Props: `lane: LaneModel`, `isSelected: boolean`,
  `hintBadge?: number`, `hintVisible: boolean`, `onSelect()`, `onOpenDiff()`,
  `onContextMenu(x: number, y: number)`.
- Move with it: `Marquee` (`Rail.tsx:283`), `StageChip` (`:343`), `LaneBuild` (`:357`) and the label
  constants `STATUS_LABEL`, `WAITING_LABEL`, `TERMINATED_LABEL` (`:39-52`).
- Preserve class names, DOM structure, titles and event handling exactly.

`src/components/Rail.tsx`
- Build a `LaneModel` per current worktree in the existing render path (a small local adapter using
  the same inputs as today: worktree info, sessions, attention, task, activity, selection) and render
  `<Lane ... />`. `Rail`'s public props are unchanged in this stage; `WorktreeMenu` stays in `Rail.tsx`.
- Remove now-unused imports and dead code.

If constructing `LaneModel` for the adapter duplicates logic that `buildGroups` already has, export a
small pure `buildLane(...)` from `railGroups.ts` (with a vitest case) and use it in both places.

## Validation

```
yarn tsc --noEmit
yarn test src/lib/laneState.test.ts src/lib/railGroups.test.ts
git diff --stat   # confirm Rail.tsx shrank and Lane.tsx is a move, not a rewrite
```
No running the app (machine-load rules); reviewers compare markup by diff.

## Commit message

```
refactor(rail): extract lane row into its own component

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

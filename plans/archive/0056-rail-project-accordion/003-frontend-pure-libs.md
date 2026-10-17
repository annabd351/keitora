# Stage 3 — Frontend pure libraries

## Goal

Land every piece of pure frontend logic (types, data hook, group model builder, expanded-set
reducer, JSON storage hook, lane-state module, pending-intent rules) with vitest coverage, without
changing any rendered UI. Proposal sections: §2, §3 (`laneState`), §4, §6 (`pendingIntent`).

## Prerequisites

- Stages 1 and 2 committed (backend wire shapes exist).

## Implementation

`src/lib/ipc.ts`
- `ProjectSummary` gains `resolved_name: string`.
- `export function reorderProjects(order: string[]): Promise<void>` invoking `reorder_projects`.

`src/lib/projectsOverview.ts` (new; wrapper lives here to avoid an `ipc.ts` / `sessions.ts` import cycle)
- `ProjectOverview` interface, `projectsOverview()` via `getTransport().invoke("projects_overview")`.
- `useProjectsOverview(): ProjectOverview[] | null`: `useLiveResource(projectsOverview, "sessions://changed", { intervalMs: 3_000, listen: liveListen })`
  like `useSessions` (`sessions.ts:131`), plus an effect subscribing to `onWorktreesChanged` and
  `onProjectChanged` that calls the resource's `reload()` through a 150 ms trailing debounce, with
  cancel-safe teardown (the `cancelled` / `un` pattern in `App.tsx`). Put the debounce in a tiny pure
  helper if it makes it testable.

`src/lib/laneState.ts` (new, pure)
- Move `blockStateCls`, `blockAnimates`, `worktreeMenuItems` and `WorktreeMenuItem` out of
  `src/components/Rail.tsx` (originally `Rail.tsx:62`, `:84`, `:404-419`); have `Rail.tsx` import them.
- `WorktreeMenuItem` gains `"focus"`; `worktreeMenuItems(terminated, opts?: { inactive?: boolean })`
  returns `["focus", ...existing]` when `inactive`.
- `git mv src/components/Rail.test.ts src/lib/laneState.test.ts`, fix its import to `./laneState`,
  keep existing assertions unchanged, add `inactive` cases (live and terminated).
- Export `DotState` here or in `railGroups.ts` (equal to `blockStateCls` output).

`src/lib/railGroups.ts` (+ `railGroups.test.ts`)
- `LaneModel`, `GroupModel`, `BuildGroupsInput`, `buildGroups` exactly as specified in §2
  (active group from rich sources when `activeReady`, otherwise and for non-active groups from the
  overview; overview matched by registry key; `activity` and `liveCounts` keyed by resolved name;
  `blockState` from `blockStateCls`; `summaryStats` definitions per §2; `focusedDot` and `overflow`
  with the focused dot taking the last visible slot when it would overflow).
- Tests (use a fixture where key differs from resolved name): active vs non-active sourcing;
  `activeReady=false` fallback to overview; waiting/busy/building/live counts; dot order and
  available-only filtering; project missing from overview gives empty lanes; `loading` when overview
  is null; `maxDots` overflow; `focusedDot` (marked, kept visible, null for non-active groups);
  non-active `attn` is `waiting` only when the session state is `waiting`.

`src/lib/railExpanded.ts` (+ `railExpanded.test.ts`)
- Immutable `ReadonlySet<string>` functions: `toggle`, `expandOnly`, `expandAll`, `collapseOthers`,
  `ensureExpanded`, `prune`. Test each, including no mutation of the input set.

`src/lib/useLocalStorageState.ts` (+ extend `useLocalStorageState.test.ts`)
- Pure `parseStoredJson<T>(raw, fallback, validate)` and hook `useLocalStorageJson<T>(key, initial, validate)`
  with the same contract as `useLocalStorageState` (read once in try/catch, write on change except
  mount render, silent on storage errors). Test parse success, null, malformed JSON, failed validate,
  mirroring the `clampStoredNumber` cases.

`src/lib/pendingIntent.ts` (+ `pendingIntent.test.ts`)
- `PendingAction`, `PendingIntent`, `SettleState`, `canConsume`, `shouldDrop` per §6.
- Tests: `canConsume` true only when target is active, `switchingTo` is null, `warmActive` is the
  target and `warmHas`; false when each single condition is missing; `shouldDrop` when the switch
  ended with a different active project and false while a switch is in flight.

Fixtures: add `resolved_name` to `ProjectSummary` fixtures in `src/lib/switchProject.test.ts` and
`src/lib/warmProjects.test.ts` (`makeSummary`, `summaryA`) so `tsc` stays green.

## Validation

```
yarn tsc --noEmit
yarn test src/lib/railGroups.test.ts
yarn test src/lib/railExpanded.test.ts
yarn test src/lib/pendingIntent.test.ts
yarn test src/lib/laneState.test.ts
yarn test src/lib/useLocalStorageState.test.ts
yarn test src/lib/switchProject.test.ts src/lib/warmProjects.test.ts src/lib/keybindings.test.ts
```

## Commit message

```
feat(rail): add pure group, expansion and pending-intent logic

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

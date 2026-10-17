# Stage 6 — Rail project groups UI

## Goal

Render every registered project as a collapsible group in the rail with live lanes, collapsed
summary, strong active/focused treatment, sticky headers and expand motion. Click-to-focus works
across projects. Drag reorder and menus come in the next stage. Proposal sections: §2 (App wiring),
§3 (new Rail props), §4, §6 (click paths), §7.

## Prerequisites

- Stages 3, 4 and 5 committed.

## Implementation

`src/App.tsx`
- Call `useProjectsOverview()` under `useProjectList()`.
- `activeProjectKey` memo (resolved name to key lookup, per §9) and
  `groups = useMemo(() => buildGroups({ ... }), [...])` with `maxDots: 8`, `liveCounts = projectSessionCounts`
  and `activeReady = warm.active === activeProjectName`.
- `onSelectLane(g, w)`: active group `setSelected(w); setView("focus")`; otherwise
  `handleSwitchProject(g.key, g.summary, { select: w, silent: true })`.
- `onOpenDiff(g, w)`: active `setDiffTarget(w)`; otherwise switch with `action: { kind: "diff" }`.
- `onNewWorktree(g)`: active `setWtOpen(true)`; otherwise switch with `action: { kind: "new-worktree" }`.
- Pass the new props to `<Rail>`; keep `onAddProject` wired to `<Rail>` as well (TopBar still has its
  tab strip until the last stage, so keep both working).

`src/components/Rail.tsx` (shell)
- Replace props per §3 (this stage only: `groups`, `selected`, `onSelectLane`, `onOpenDiff`,
  `onNewWorktree`, `onAddProject`, plus the unchanged daemon/menu props). Active-lane menu path is
  unchanged. Do not add `onReorder`/`onLaneAction` yet.
- Label row: "Projects", cycle hint `{modKeyLabel} ← →` (when `hintVisible`) before the `+` button,
  `+` becomes "Add project" calling `onAddProject`.
- `expanded` state via `useLocalStorageJson("keitora:railExpanded", null, validate)` and the
  derived `Set`; effects for first-run seed, auto-expand on active-key change (ref initialised to
  the first non-null key), and `prune` only when `groups.length > 0` (§4).
- Scroll-into-view effect for `.lane-row.sel` on `selected`/active-key change, respecting reduced
  motion via `usePrefersReducedMotion`.

`src/components/rail/ProjectGroup.tsx` (new) and a minimal `RailProjects.tsx`
- `RailProjects` renders one `ProjectGroup` per `GroupModel` (no dnd yet; the container structure
  `.rail-projects > .rail-group` is final so the next stage only wraps it).
- `ProjectGroup`: sentinel + `IntersectionObserver` for `data-stuck`, header with sibling
  `button.rail-group-toggle` (chevron, name, wait dot, dot strip with focused dot and `+N`, work dot,
  count) and `.rail-group-actions` (per-group `+` New worktree now; grip and `⋯` arrive next stage),
  body using the grid-row collapse, `aria-expanded`/`aria-controls`/`aria-current`, empty states
  ("Loading..." / "No worktrees"). Lanes use `Lane` with `isSelected = g.isActive && lane.name === selected`
  and `hintBadge` for the active group only.
- Extract any non-trivial derivations (dot visibility, stuck decision, aria values) into pure helpers
  in `src/lib/` with vitest if they contain logic.

`src/App.css`
- New `/* ---- rail project groups ---- */` section with the classes and rules in §7 ("Implementation
  specifics"): group/head/toggle/actions/body, active header treatment, waiting edge, dots, wait
  dot with `pulse`, focused-lane rules (`.lane-row.sel .lane`, `::before` bar, name weight), sticky
  and stuck rules, expand motion, reduced-motion overrides, light-theme shadow, `scroll-padding-top: 34px`
  on `.rail-lanes`, lane indent. Use existing tokens only; both themes.
- Do not yet remove `.project-tab*` / `.project-cycle-hint` (TopBar still uses them until the last stage).

## Validation

```
yarn tsc --noEmit
yarn test src/lib/railGroups.test.ts src/lib/railExpanded.test.ts src/lib/useLocalStorageState.test.ts
yarn test   # one full frontend run is fine here; it is fast and node-only
```
No app launch. Visual checks are deferred to the smoke phase.

## Commit message

```
feat(rail): show projects as collapsible groups with live lanes

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

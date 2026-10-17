# Stage 7 — Drag reorder, project menu, non-active lane menu

## Goal

Add drag-and-drop (and keyboard) reordering of project groups persisted through `reorder_projects`,
the project header context menu (including Reveal and Remove), and the non-active lane context menu
with a leading Focus item. Proposal sections: §5 (Frontend), §6 (right-click on non-active lane), §8.

## Prerequisites

- Stage 6 committed.

## Implementation

`src/components/rail/RailProjects.tsx`
- `DndContext` + `SortableContext` (`verticalListSortingStrategy`) per §5: `PointerSensor` with
  `activationConstraint: { distance: 4 }`, `KeyboardSensor` with `sortableKeyboardCoordinates`,
  `closestCenter`, `MeasuringStrategy.Always`. ids are project keys.
- `.dragging` class on `.rail-projects` during a drag; optimistic `orderOverride` cleared when the
  `groups` order catches up or on rejection (`showToast(String(e))`).
- `onDragEnd`: `arrayMove` then `onReorder(keys)`. Keep the order computation in a pure helper
  (for example `reorderKeys(keys, activeId, overId)` in `src/lib/railOrder.ts`) with vitest cases:
  move up/down, same position, unknown id, and "override matches incoming order".

`src/components/rail/ProjectGroup.tsx`
- `useSortable` with the vertical-only transform and reduced-motion transition (§5); spread
  `listeners` on `.rail-group-head`; `setActivatorNodeRef` + `attributes` on the new
  `button.rail-group-grip` (`GripVertical`, `aria-label="Reorder <key>"`); `⋯` button
  (`button.rail-group-more`) and header `onContextMenu` open the project menu.
- `.is-dragging` styling.

`src/components/Rail.tsx`
- New props: `onReorder(keys)`, `onLaneAction(g, worktree, action)`.
- `ProjectMenu` (second Radix DropdownMenu with the 0x0 anchor pattern, state `{ key, x, y } | null`,
  single instance): New worktree... (`onNewWorktree`), Reveal in file explorer
  (`await import("@tauri-apps/plugin-opener")` then `revealItemInDir(g.summary.root)`, errors to
  toast; do NOT use `openPath`), Collapse others, Expand all (reducer functions), Remove project...
  (`useConfirmDialog`, `removeProject(key)` from `src/lib/settings.ts`, toast on success/error,
  disabled with `title="Switch to another project first"` for the active group).
- `WorktreeMenu` state gains `projectKey: string; inactive: boolean`; items from
  `worktreeMenuItems(terminated, { inactive })`. Active lanes call existing handlers; inactive lanes
  call `onLaneAction(g, w, action)` and `Focus` calls `onSelectLane`.

`src/App.tsx`
- `onReorder = (keys) => reorderProjects(keys)`.
- `handleLaneAction(g, w, a)`: active `runPendingAction(a, w)`; otherwise
  `handleSwitchProject(g.key, g.summary, { select: w, silent: true, action: a })`.

`src/App.css`
- Grip/actions/more visibility, `.rail-projects.dragging .rail-group-body` collapse, `.is-dragging`
  header lift (light theme included).

## Validation

```
yarn tsc --noEmit
yarn test src/lib/railOrder.test.ts src/lib/laneState.test.ts src/lib/railExpanded.test.ts
yarn test
git grep -nE '#[0-9]{3,4}' -- src
```
The last command must print nothing for changed files. No app launch.

## Commit message

```
feat(rail): reorder projects and add project and lane context menus

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

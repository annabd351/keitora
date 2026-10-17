# 0058 — Rail project groups: stable drag reorder + distinct project sections

Follow-up to the rail project accordion (archived at `plans/archive/0056-rail-project-accordion/`).

## Goal

1. **Drag reorder keeps the layout still.** Today a drag collapses every group body
   (`.rail-projects.dragging .rail-group-body` in `src/App.css`), so an expanded project snaps
   shut the instant a drag starts. The list jumps under the pointer and you lose your place.
   One project is almost always open, so this happens on nearly every reorder.
2. **Projects read as distinct sections.** Groups currently look like slightly larger lane
   rows. Each project should look like its own thing: an identity color, a card container,
   and a header that says more (main worktree path, last activity) while keeping the
   colored status dots.

## Scope

**In**
- Reorder model: frozen layout + insertion line (pointer and keyboard).
- Per-project identity hue (derived from the registry key), monogram tile, and card styling for
  open and collapsed groups, in dark and light themes, with reduced motion respected.
- Header details: the main worktree path (`ProjectSummary.root`, home shortened to `~`) and
  last activity (relative time). The existing dot strip, needs-input ring, work dot and live
  count stay.

- **Task info for every project** (added mid-implementation). Today a lane's active-task
  line only shows for the active project, because only that project's task list is loaded.
  `projects_overview` now also returns each project's active tasks
  (`LocalStore::active_tasks`, lowest id per worktree, read through the same read-only DB
  handle). `buildGroups` puts them on the non-active groups' lanes. Projects on the
  GitHub task backend keep no tasks in the local DB, so their non-active lanes still show
  no task line.

**Out**
- A user-chosen project color (a picker or `keitora.toml` key). The hue is hashed for now.
  The CSS variable is the hook a later override would set.
- A true "last state change" timestamp in the backend (see Risks).
- Changes to lane rows (`Lane.tsx`) beyond what the card container implies (indent, spine).

## Architecture

### 1. Reorder: frozen layout + insertion line

Replace the sortable model (`useSortable` and `verticalListSortingStrategy` with every body
collapsed) with **plain draggables and a computed insertion index**:

- `RailProjects` keeps `DndContext` and `DragOverlay`. Each header uses `useDraggable`, not
  `useSortable`, so nothing gets a transform and nothing shifts. Sticky headers keep working
  mid-drag. (`@dnd-kit/sortable` stays a dependency: the Board, `DispatchStepsEditor` and
  the `arrayMove` in `railOrder.ts` use it.)
- **No `.dragging` collapse.** Open groups stay open. The dashed "well" treatment of the
  dragged header goes away. Instead the dragged group's whole card dims to ~45% opacity in
  place (`.is-dragging`). Only the grip stays at full strength, so a keyboard drag keeps
  a visible focused handle. The lifted chip (`ProjectGroupOverlay`) follows the pointer as
  today, as a **compact single-line** header (monogram, name, dots, count; no subtitle).
- **Insertion index** is a pure function in a new `src/lib/railDrop.ts`:
  `insertionIndex(heads: {top, bottom}[], y: number): number`. It returns a slot index
  `0..n`: the first group whose **header** midpoint lies below `y` gives the slot before
  it, and if there is none the slot is `n`. It uses header midpoints, not whole-card
  midpoints. With the chip anywhere over an open group's lanes you are already past its
  header, so the slot is "after it". Moving past a tall open group only needs the chip to
  clear its header, which keeps drags short.
- **Coordinates.** `y` is the overlay's current vertical centre in viewport space:
  the centre of `active.rect.current.initial` plus `delta.y` (from `onDragMove`). The
  overlay is `position: fixed` and moves only by `delta`. So `y` matches the header rects
  re-read with `getBoundingClientRect()` on each move, even after the rail auto-scrolls.
  Do not use `translated`, which folds in a scroll adjustment. Pointer and keyboard drags
  share this one path. Auto-scroll is checked by hand.
- **The drop line** is a 2px accent bar with small end caps, drawn in the gap between
  cards at that slot. It is a positioned element inside `.rail-projects`, which becomes
  `position: relative`. Its `top` is the gap midpoint between card `slot-1`'s bottom and
  card `slot`'s top, relative to `.rail-projects` (`lineOffset(cards, slot, listTop)`,
  also pure). The first and last slots sit just above the first card and just below the
  last. A slot that would leave the order unchanged
  (the dragged group's own index or index+1) shows no line. A drop there is a no-op.
- **Drop:** `moveToSlot(keys, activeKey, slot)` (pure, in `railOrder.ts` next to
  `reorderKeys`) returns the new key order or `null` for a no-op. It feeds the existing
  optimistic `override` / `onReorder` / revert-with-toast path unchanged.
- **Keyboard:** keep dnd-kit's `KeyboardSensor`, so pickup, drop, Escape, focus return and
  `accessibility.announcements` stay with dnd-kit. Replace only `sortableKeyboardCoordinates`
  with a custom `coordinateGetter`. On ArrowUp/ArrowDown it:
  - computes the current slot;
  - steps to the next slot that changes the order (`stepSlot(slot, dir, from, n)`, pure;
    it skips the two no-op slots and clamps at the ends);
  - returns coordinates that put the overlay's centre just inside that slot's zone
    (`slotTargetY(heads, slot)`, pure).

  The move then flows through the same `onDragMove` → `insertionIndex` path as a pointer
  drag, with no separate keyboard state. Announcements are rewritten to name the slot:
  "keitora will move to position 2 of 4." On keyboard moves,
  `scrollIntoView({ block: "nearest" })` keeps the line in view.
- **Auto-scroll** while dragging near the rail's edges stays on dnd-kit's built-in
  `autoScroll`. Rects are re-measured on scroll (one `getBoundingClientRect` pass per
  `onDragMove`; the list is small).

Why not keep sortable with full-height items? Moving past a tall open group then means a long
drag and big jumps. The insertion line keeps every pixel where it was, which was the
point of the request.

### 2. Distinct sections

**Identity hue.** `projectHue(key: string): number` (pure, in a new `src/lib/projectHue.ts`)
hashes the registry key (FNV-1a) onto a fixed set of ~10 hue angles. The angles are spaced
to avoid clashing with the status colors (waiting blue, busy, the accent gold), so a
project color is never mistaken for a status. `ProjectGroup` sets `--proj-h: <deg>` on
`.rail-group`. CSS builds the shades with `oklch()` per theme:

- `--proj` (the tile and spine): mid lightness, moderate chroma.
- `--proj-tint` (the card surface wash): very low chroma.
- Light theme gets its own L/C values.
- Each is declared first as an `hsl(var(--proj-h) …)` fallback, then as `oklch()`, so
  older WKWebViews (the macOS 10.15 floor) still get a color.

**Card.** Each `.rail-group` becomes a card: a 1px `--line` border, `--radius-chip`, a
faint `--proj-tint` background, and 6px between cards (up from 3px). Sticky still works
because the group gets no overflow, contain or transform; the earlier constraint holds.
- Collapsed: a single row the height of the header.
- Open: header plus lanes, with a 2px `--proj` **spine** down the left of the lane list,
  inset under the monogram.
- Active project: keeps its accent selection treatment (border, pill marker and tint stay
  on the header). The card border picks up `--sel-edge`, so "selected" still reads
  differently from "identity".

**Header layout.** The header grows to two lines, 44px tall on collapsed groups too, for a
consistent rhythm. The hardcoded `height: 34px` on `.rail-group-head` changes:

```
[K]  keitora                 ● ● ○  ⟳  3
     ~/repos/lodi · 2m ago
```

- **Monogram tile**: an 18px rounded square filled with `--proj` and showing the key's first
  letter (uppercased). It takes the chevron's place and doubles as it. On header hover, or
  when the toggle has `:focus-visible`, the letter cross-fades to the chevron (rotated 90°
  when open). With reduced motion the swap is instant. The needs-input ring keeps its slot
  right after the name.
- **Subtitle line**: the shortened root path (left-ellipsised so the repo folder name
  survives, mono, `--tx-3`), then ` · ` and the last-activity text. A full-path `title`
  tooltip shows on hover.
- The right cluster (dots, work dot, count) is unchanged and stays vertically centred on
  line 1. The dots still hide when the group is open (the lanes show them).
- The pinned (sticky) state keeps its flat top and shadow.

**Last activity.** `LaneModel` gains `lastHeartbeat: string | null`, set in `buildLane`
from the session it already receives (`""` or no session gives `null`). `SummaryStats`
gains `lastActivity: string | null`: the max `lastHeartbeat` across the group's lanes.
The fixed-width `YYYY-MM-DD HH:MM:SS` format compares correctly as a string. This is available for every project:
`sessions_list` for the active project and `projects_overview` for the rest. A new pure
helper `formatAgo(isoUtc, nowMs)` in `src/lib/format.ts` produces `now` (<1m), `5m`, `3h`,
`2d`, and falls back to `—`. The value is SQLite UTC `YYYY-MM-DD HH:MM:SS` without a zone,
so it is parsed as UTC. When `busy + building > 0`, the text is `active` instead,
since the heartbeat understates a long busy turn. `RailProjects` owns one minute-aligned
`now` tick shared by all groups, so labels age without per-group timers.

**Path.** `shortPath(root, home)` (pure) replaces a leading home dir with `~` and
normalises separators to `/` for display. The prefix match is case-insensitive on Windows.
Home comes once from `homeDir()` in `@tauri-apps/api/path`, which `core:default` grants
through `core:path:default`. A tiny hook caches it and catches failures (non-Tauri runs,
tests) by falling back to the raw path. Until it resolves, the raw path shows.

## Technical design: files touched

| File | Change |
|---|---|
| `src/lib/railDrop.ts` (+test) | new: `insertionIndex`, `lineOffset`, keyboard slot stepping |
| `src/lib/railOrder.ts` (+test) | `moveToSlot` |
| `src/lib/projectHue.ts` (+test) | new: key → hue angle |
| `src/lib/format.ts` (+test) | `formatAgo`, `shortPath` |
| `src/lib/railGroups.ts` (+test) | `LaneModel.lastHeartbeat` in `buildLane`; `lastActivity` in `summarize` / `SummaryStats`; `shareGroups` unaffected (plain data) |
| `src/lib/railGroupView.ts` (+test) | `headerView` gains `subtitle` parts (path, ago/active); test fixtures `lane()` / `stats()` gain the new fields |
| `src/components/rail/RailProjects.tsx` | draggable model, slot state, drop line, keyboard `coordinateGetter`, announcements, `now` tick, home dir |
| `src/components/rail/ProjectGroup.tsx` | `useDraggable`, monogram, two-line header, `--proj-h` |
| `src/App.css` | remove drag-collapse rules; drop line; card, monogram, spine, subtitle; light theme; reduced motion |

Rust changes for the task info only: `ActiveTask` + `LocalStore::active_tasks`
(`crates/keitora-tasks`), and `ProjectOverview.active_tasks` (`src-tauri/src/host.rs`).

## Dependencies

- `@dnd-kit/core` 6.3.1: `useDraggable` and `KeyboardSensor` (installed). `@dnd-kit/sortable`
  stays (used elsewhere).
- `@tauri-apps/api/path` `homeDir` (already a dependency via `@tauri-apps/api`).

## Risks

- **Last activity is a proxy.** `last_heartbeat` updates on Claude hook events (each turn),
  not on PTY output. The `active` override covers busy lanes. A Codex worktree without hooks
  may never update and will show stale times or `—`. That is acceptable for now. A proper
  `state_changed_at` column is the follow-up if it proves misleading.
- **Taller headers** (34px to about 44px) cost vertical density with many projects. Mitigation:
  keep the subtitle at 11px, and accept that or drop the subtitle on collapsed groups at
  review.
- **Keyboard reorder**: a custom `coordinateGetter` replaces `sortableKeyboardCoordinates`.
  dnd-kit still owns focus, Escape and announcements. The pure slot-stepping is
  unit-tested. The DOM part needs a manual check.
- **Re-renders**: the minute `now` tick re-renders every (memoized) group once a minute, and
  `lastActivity` changes on each hook event. Both are cheap at rail scale.
- **Hue clashes**: two projects can hash to the same hue. That is fine; the name is still the
  identifier.

## Implementation notes

- **Drag tracking reads the DOM.** The slot comes from the lifted copy's on-screen rect,
  re-read every animation frame while a drag runs. It is not derived from the
  `initial + delta` of `onDragMove`. dnd-kit's deltas fold in scroll adjustments, and the
  per-frame read also keeps the line right while the rail auto-scrolls under a still
  pointer. The keyboard getter moves the copy by `target - current centre`.
- **A keyboard drag hides the copy.** It stays laid out (and measured) but invisible, so only
  the dimmed group and the line show. A pointer drag keeps the copy, and the line draws
  above it.
- **"active"** is any lane with a busy session, background work or a build
  (`SummaryStats.active`), not just `busy + building`. A busy session alone has no
  activity flag.
- Hues are OKLCH angles chosen to avoid the status hues. The hsl fallback (outside
  `@supports (color: oklch(...))`) is approximate.

## Resolved questions

1. Chevron placement: the monogram tile cross-fades to the chevron on hover or focus-visible.
2. Subtitle on collapsed groups: yes (44px everywhere). Revisit after the visual check.

# 0059 — Rail project headers: breathing room + a readable path

## Goal

Project group headers in the rail look cramped, and the path subtitle usually clips down to
just the folder name (`…my-game`, `…s/shared-tools`), so you can't tell where a project
lives. Give the headers more height and spacing, widen the rail a touch, and make the path
show at least two levels.

## Scope

**In**
- Rail width: `--rail-width` 272px → **304px** (~12% wider).
- More breathing room: header padding instead of a fixed 44px height (open ≈ 54px,
  collapsed ≈ 66px with the dot row); row gap 2px → ~4px; card gap 6px → ~10px.
- Header layout, three rows beside the monogram tile:
  1. **name** (+ needs-input dot) … work dot · count, right-aligned
  2. **path · last activity**, full text width
  3. **worktree status dots**, on their own row, only when collapsed (an open group shows
     its lanes, so it has no dot row, as today)

  Nothing on rows 2–3 competes for width any more.
- Path display rule: show `root + 2 folders` in full (`D:/repos/lodi`,
  `~/oss/some-tool`, `D:/scratch`); deeper paths show only the **trailing two
  folders** (`C:/Users/x/src/repos/shared-tools` → `repos/shared-tools`). The full
  path stays in the tooltip; the existing left-clip ellipsis remains as a last resort.

**Out**
- Lane cards (worktree rows) — unchanged apart from inheriting the wider rail.
- Making the rail user-resizable.
- Board lanes (`src/components/board/Lane.tsx`'s `max-w-[272px]` is the board column, unrelated).

## Architecture / Technical design

- `src/App.css`
  - `:root --rail-width` → 304px.
  - `.rail-group-head`: drop the fixed `height: 44px` for vertical padding + `min-height`
    so it grows with the dot row; `.rail-group + .rail-group` margin → ~10px.
  - `.rail-group-toggle`: grid `tile | text` where `.rail-group-text` stacks title row,
    subtitle row and dot row. The title row holds name, needs-input dot, then a
    right-aligned work-dot + count. The tile aligns to the top (first row), not centred,
    so it doesn't drift when the dot row appears.
  - Hover actions (`.rail-group-actions`) overlay the right end of the **title row** only
    (no longer full height), so they cover the count and never the path or dots.
    Concretely: `.rail-group-actions` drops `bottom: 0` for `height` = the title row's
    height (tracked by a `--head-row` custom property shared with the title row), keeping
    the gradient fade into `--head-bg`. The drag-state and menu-open `.rail-group-actions`
    rules keep working since they only touch opacity/pointer-events.
  - Head sizing: the head keeps `display: flex` with `align-items: stretch`; the toggle
    drops `height: 100%` (only resolved against the fixed 44px) and carries the vertical
    padding itself, so the whole header stays clickable.
  - Sticky offset: `.rail-lanes { scroll-padding-top }` is a stale 34px (the header is
    already 44px), so `scrollIntoView({block:"nearest"})` on the focused lane
    (`Rail.tsx`) can tuck it under a pinned header. Set it to the open header's height
    (+ a few px) and fix its comment. Open headers are what pin over lanes, so the open
    height is the right one.
  - The active pill (`::before`, `top/bottom: 6px`) scales with the box; it reads fine at
    both heights. Spot-check in the smoke.
  - The `.rail-group-meta` "fixed slots" comment is reworded for the split layout.
- **Drag overlay.** `ProjectGroupOverlay` reuses `HeaderSummary` (with `subtitle={null}`,
  `open={false}`) inside a fixed 36px single-line chip (`.rail-group-overlay
  .rail-group-head`). The chip stays single-line: no subtitle (as today) and the overlay's
  CSS hides the dot row, so it renders name + work dot + count only. Its height comes
  from `--head-row`. dnd-kit sizes the overlay box to the grabbed header, now taller than
  the chip, so `.rail-group-overlay` fills that box and centres the chip: the measured
  centre (`overlayCentre`) starts where the header's was. Drag insertion math
  (`RailProjects.tsx`: `insertionIndex`, `slotTargetY`, `lineOffset`) reads live rects,
  so it adapts. Covered by the drag smoke.
- Header sizes (`--head-pad-t`, `--head-row`, `--head-gap`, `--head-sub`, `--head-pad-b`)
  live on `.rail`, so the tile, rows and `.rail-lanes` scroll padding derive from one
  source.
- `src/components/rail/ProjectGroup.tsx` — `HeaderSummary` markup: the work dot + count
  join the title row in an `aria-hidden` `.rail-group-meta`; the dot strip becomes its own
  `aria-hidden` `.rail-group-dots-row` after the subtitle (both stay hidden from AT; the
  toggle's accessible name from `groupAriaLabel` is unchanged). `subtitle.path` becomes
  `tailPath(shortPath(root, home))`. `headerView` (`src/lib/railGroupView.ts`) is
  unchanged.
- `src/lib/format.ts` — new pure `tailPath(path, levels = 2)` applied after `shortPath`
  (input is already `/`-separated):
  - Trailing slashes are ignored (`D:/repos/` ≡ `D:/repos`).
  - A Windows verbatim prefix (`//?/`, `//?/UNC/`) is stripped first.
  - Root: `~`, a drive (`D:`), a UNC share (`//server/share`), or the POSIX `/`.
  - If the path has ≤ `levels` folders after its root, return it unchanged (`~`,
    `D:/scratch`, `D:/repos/lodi`, `//srv/share/a/b`, `/a/b`). Otherwise return the last
    `levels` folders joined (`C:/Users/x/src/repos/shared-tools` →
    `repos/shared-tools`).
  - Unit tests in `src/lib/format.test.ts`: home-relative, drive, POSIX root, UNC,
    verbatim, exactly-at-threshold, one past it, trailing slash, `~` alone, empty,
    `levels` override.
- `home` is null until Tauri's `homeDir()` resolves (`useRailClock.ts`), so the subtitle
  can flip once from the tail of the raw path to the `~`-relative form. This already
  happens today with `shortPath`; it's accepted.

## Dependencies

None. Builds on the rail project groups.

## Risks

- Taller headers mean fewer projects fit in the rail at once (~20px more per collapsed
  project). That is the intended trade.
- Header height now differs open vs collapsed (~12px), so it jumps once on toggle; it
  rides the existing body open/close animation, so it should read as one motion. Verify.
- The grid change touches the sticky header; verify pinning, the active pill, the
  needs-input edge, hover actions and drag in both themes.
- A wider rail takes ~32px from the terminal area.

## Open questions

- None blocking; exact pixel values get tuned during the visual smoke.

## Complexity

**Small**: 3 files, one subsystem. Skip Fracture / Singletonize / Loss check.

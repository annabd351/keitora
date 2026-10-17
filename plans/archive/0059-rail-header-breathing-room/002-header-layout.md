# Stage 2 — header layout + rail width

**Goal:** a roomier three-row project header and a 304px rail.

**Prerequisites:** Stage 1.

**Implementation:**
- `src/App.css`:
  - `--rail-width: 304px`.
  - Card gap: 10px.
  - The head drops its fixed height; the toggle carries the padding.
  - Title row: name, wait dot, then meta (work dot + count) right-aligned.
  - Subtitle row: full width.
  - Dot row on its own.
  - The tile is top-aligned.
  - Actions are sized to the title row.
  - Fix the `.rail-lanes` `scroll-padding-top` value and its comment.
  - Update the overlay rules so the chip stays single-line.
- `src/components/rail/ProjectGroup.tsx`:
  - `HeaderSummary` is restructured.
  - Add a `compact` prop for the overlay.
  - Path becomes `tailPath(shortPath(...))`.

**Validation:**
- `tsc --noEmit` and `yarn test` green.
- App smoke via a screenshot of the rail: collapsed, open, active, pinned, hover actions,
  drag overlay, and light theme.

**Commit:** `feat(rail): roomier project headers with full-width path and dot rows`

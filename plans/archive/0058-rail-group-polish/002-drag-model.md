# 002 — Drag model

**Goal:** dragging never collapses or shifts groups; an insertion line marks the drop slot.

**Implementation:** `ProjectGroup` uses `useDraggable` (no transform on the header);
`RailProjects` measures headers/cards on drag start/move, derives the slot from
`initial centre + delta.y`, renders the drop line, drops via `moveToSlot`, keyboard via a
custom `coordinateGetter` built on `stepSlot`/`slotTargetY`; announcements speak slots.
CSS: delete the `.dragging` collapse rules and the dashed well; dim `.is-dragging`.

**Validation:** `yarn test`, `yarn tsc --noEmit`, `yarn lint` (biome).

**Commit:** `fix(rail): keep groups open and still while reordering`

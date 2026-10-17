# 001 — Pure logic

**Goal:** all derivations the UI needs, unit-tested, no component changes beyond type fallout.

**Implementation**
- `src/lib/railDrop.ts`: `insertionIndex(heads, y)`, `lineOffset(cards, slot, listTop)`,
  `stepSlot(slot, dir, from, n)`, `slotTargetY(heads, slot)`, `isNoopSlot(slot, from)`.
- `src/lib/railOrder.ts`: `moveToSlot(keys, activeKey, slot)` → new order or `null`.
- `src/lib/projectHue.ts`: `projectHue(key)` → one of a fixed hue set (FNV-1a).
- `src/lib/format.ts`: `formatAgo(utc, nowMs)`, `shortPath(root, home)`.
- `src/lib/railGroups.ts`: `LaneModel.lastHeartbeat`, `SummaryStats.lastActivity`.
- `src/lib/railGroupView.ts`: `headerView` gains `ago` ("active" | relative | null).

**Validation:** `yarn test`, `yarn tsc --noEmit`.

**Commit:** `feat(rail): drop-slot, hue and last-activity helpers`

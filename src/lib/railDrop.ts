// Where a dragged rail project group would land. The rail never moves or collapses groups
// mid-drag; instead it measures them and works out an insertion slot (0..n, the gap
// before group `slot`) from the lifted chip's vertical centre, then draws a line in that
// gap. Pure, over viewport rects, so it can be unit-tested without rendering.

export interface VSpan {
  top: number;
  bottom: number;
}

const mid = (r: VSpan) => (r.top + r.bottom) / 2;

/**
 * The slot for a chip centred at `y`, given each group's header rect in order: before the
 * first header whose midpoint lies below `y`, else after the last. Headers, not whole
 * cards: over an open group's lanes the chip is already past its header, so it lands
 * after that group, which keeps drags past a tall group short.
 */
export function insertionIndex(heads: readonly VSpan[], y: number): number {
  for (let i = 0; i < heads.length; i++) {
    if (y < mid(heads[i])) return i;
  }
  return heads.length;
}

/** A slot that leaves the group at index `from` where it already is. */
export function isNoopSlot(slot: number, from: number): boolean {
  return slot === from || slot === from + 1;
}

/** The index the group at `from` ends up at when dropped into `slot`. */
export function landingIndex(slot: number, from: number): number {
  return slot > from ? slot - 1 : slot;
}

/** How far outside the first or last card the drop line sits, in px. */
const EDGE = 3;

/**
 * The drop line's vertical centre, relative to the list's top `listTop`, for `slot` among
 * the group cards: the middle of the gap between cards, or just outside the first or last
 * card. `null` with no cards.
 */
export function lineOffset(cards: readonly VSpan[], slot: number, listTop: number): number | null {
  const n = cards.length;
  if (n === 0) return null;
  const s = Math.max(0, Math.min(n, slot));
  if (s === 0) return cards[0].top - EDGE - listTop;
  if (s === n) return cards[n - 1].bottom + EDGE - listTop;
  return (cards[s - 1].bottom + cards[s].top) / 2 - listTop;
}

/**
 * The next slot from `slot` in direction `dir` that actually moves the group at `from`,
 * among `n` groups: the two slots around the group itself are skipped. Stays at `slot`
 * when there is nowhere further to go.
 */
export function stepSlot(slot: number, dir: 1 | -1, from: number, n: number): number {
  for (let s = slot + dir; s >= 0 && s <= n; s += dir) {
    if (!isNoopSlot(s, from)) return s;
  }
  return slot;
}

/**
 * A chip centre `y` that `insertionIndex` maps to `slot`: halfway between the midpoints
 * of the headers either side, or a header's outer edge at either end. `null` with no
 * headers.
 */
export function slotTargetY(heads: readonly VSpan[], slot: number): number | null {
  const n = heads.length;
  if (n === 0) return null;
  const s = Math.max(0, Math.min(n, slot));
  if (s === 0) return heads[0].top;
  if (s === n) return heads[n - 1].bottom;
  return (mid(heads[s - 1]) + mid(heads[s])) / 2;
}

// Ordering of the rail's project groups while a drag reorder is in flight. The rail shows
// a dropped order straight away (an "override") and keeps it until the registry's own
// order catches up, or drops it when the registry changes under it. Pure, keyed by
// registry key.

import { arrayMove } from "@dnd-kit/sortable";
import { isNoopSlot, landingIndex } from "./railDrop";

/** Whether `a` and `b` hold the same keys in the same order. */
export function sameOrder(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((k, i) => k === b[i]);
}

/** Whether `a` and `b` hold the same set of keys, in any order. */
export function sameKeys(a: Iterable<string>, b: Iterable<string>): boolean {
  const as = new Set(a);
  const bs = new Set(b);
  return as.size === bs.size && [...bs].every((k) => as.has(k));
}

/**
 * The order after dropping `activeId` into insertion `slot` (the gap before the key at
 * that index; `keys.length` is the end), or `null` when nothing moves (the slot right
 * before or after the key itself, out of range, or the key unknown).
 */
export function moveToSlot(
  keys: readonly string[],
  activeId: string,
  slot: number,
): string[] | null {
  const from = keys.indexOf(activeId);
  if (from === -1 || slot < 0 || slot > keys.length) return null;
  if (isNoopSlot(slot, from)) return null;
  return arrayMove([...keys], from, landingIndex(slot, from));
}

/**
 * The override still worth showing once the registry reports `incoming`: `null` when the
 * registry already has that order (it caught up) or a different set of projects (one was
 * added or removed, so the dropped order is stale); otherwise the override itself.
 */
export function settleOverride(
  override: readonly string[] | null,
  incoming: readonly string[],
): readonly string[] | null {
  if (override === null) return null;
  if (sameOrder(override, incoming) || !sameKeys(override, incoming)) return null;
  return override;
}

/**
 * `items` in `order` (by `keyOf`). Items missing from `order` keep their relative order
 * at the end; keys in `order` with no item are skipped.
 */
export function applyOrder<T>(
  items: readonly T[],
  order: readonly string[] | null,
  keyOf: (item: T) => string,
): T[] {
  if (order === null) return [...items];
  const byKey = new Map(items.map((it) => [keyOf(it), it]));
  const out: T[] = [];
  for (const k of order) {
    const it = byKey.get(k);
    if (it !== undefined) {
      out.push(it);
      byKey.delete(k);
    }
  }
  for (const it of items) if (byKey.has(keyOf(it))) out.push(it);
  return out;
}

/**
 * Wrap `persist` so calls run one at a time, in call order, and a call made while one is
 * in flight replaces any call still waiting: rapid drops land in order and the latest
 * one wins. A replaced call resolves without running; the others settle with their own
 * run's outcome.
 */
export function latestWins<A>(persist: (arg: A) => Promise<void>): (arg: A) => Promise<void> {
  type Waiting = { arg: A; resolve: () => void; reject: (e: unknown) => void };
  let busy = false;
  let waiting: Waiting | null = null;
  const run = async ({ arg, resolve, reject }: Waiting) => {
    busy = true;
    try {
      await persist(arg);
      resolve();
    } catch (e) {
      reject(e);
    }
    const next = waiting;
    waiting = null;
    if (next === null) busy = false;
    else void run(next);
  };
  return (arg) =>
    new Promise<void>((resolve, reject) => {
      if (!busy) {
        void run({ arg, resolve, reject });
        return;
      }
      waiting?.resolve();
      waiting = { arg, resolve, reject };
    });
}

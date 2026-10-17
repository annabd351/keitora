// Which rail project groups are expanded, as an immutable set of registry keys. No
// function mutates its input, so the result can go straight into React state.

import { sameKeys } from "./railOrder";

/** Open `key` if closed, close it if open. */
export function toggle(s: ReadonlySet<string>, key: string): ReadonlySet<string> {
  const next = new Set(s);
  if (next.has(key)) next.delete(key);
  else next.add(key);
  return next;
}

/** Open every group in `keys` (keeping any already open). */
export function expandAll(s: ReadonlySet<string>, keys: readonly string[]): ReadonlySet<string> {
  return new Set([...s, ...keys]);
}

/** Close every group except `key`, which ends up open (the menu's "Collapse others"). */
export function collapseOthers(key: string): ReadonlySet<string> {
  return new Set([key]);
}

/** Open `key`; returns the input itself when it is already open, so state does not churn. */
export function ensureExpanded(s: ReadonlySet<string>, key: string): ReadonlySet<string> {
  if (s.has(key)) return s;
  return new Set([...s, key]);
}

/** Drop a removed project's key; returns the input itself when it was not open. */
export function forget(s: ReadonlySet<string>, key: string): ReadonlySet<string> {
  if (!s.has(key)) return s;
  const next = new Set(s);
  next.delete(key);
  return next;
}

/**
 * Validator for the persisted value: the open keys, or `null` when nothing has been stored
 * yet (first run, which seeds the active project open).
 */
export function isStoredExpanded(v: unknown): v is string[] | null {
  return v === null || (Array.isArray(v) && v.every((x) => typeof x === "string"));
}

/**
 * Apply a set update to the persisted value. Returns `prev` itself when the set did not
 * change, so state does not churn, and keeps `null` (never stored) when an update to it
 * leaves nothing open, so a removal before the first-run seed does not cancel the seed.
 */
export function applyExpanded(
  prev: string[] | null,
  update: (s: ReadonlySet<string>) => ReadonlySet<string>,
): string[] | null {
  const cur = new Set(prev ?? []);
  const next = update(cur);
  if (prev === null && next.size === 0) return null;
  if (prev !== null && sameKeys(next, cur)) return prev;
  return [...next];
}

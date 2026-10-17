// Pure tag-filter helpers for the Board's Open (unassigned) lane. Kept free of
// React so the filtering rules are trivially unit-testable — mirrors the
// buildLanes / useBoardLanes split, with the logic in a plain module and a thin
// hook/component consuming it.

import type { TaskWithDeps } from "../../lib/tasks";

/**
 * Union of tags across the given tasks, de-duplicated and sorted
 * case-insensitively (locale-aware). Drives the Open lane's filter options; we
 * derive it live from the lane's own tasks so a tag that no longer appears just
 * drops out of the menu.
 */
export function availableTags(tasks: TaskWithDeps[]): string[] {
  const set = new Set<string>();
  for (const t of tasks) {
    for (const tag of t.tags) set.add(tag);
  }
  // Case-insensitive primary sort, with a case-sensitive tiebreak so tags that
  // differ only in case (e.g. "Bug"/"bug") get a deterministic, stable order
  // instead of rendering as two order-ambiguous look-alike options.
  return [...set].sort(
    (a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }) || a.localeCompare(b),
  );
}

/**
 * Narrow a selection to the tags that still exist in `available`.
 *
 * Keeps the Open lane's filter honest as its tasks change: a selected tag that
 * disappears from the lane stops filtering (rather than hiding every card with the
 * Clear control gone) and is dropped from state so it can't silently resurrect if
 * that tag later returns. Also prevents a selection made in one project from
 * carrying over and filtering a different project's Open column. Preserves the
 * order of `selected`.
 */
export function activeSelection(selected: string[], available: string[]): string[] {
  if (selected.length === 0) return selected;
  const have = new Set(available);
  return selected.filter((tag) => have.has(tag));
}

/**
 * Filter tag *options* by a search query (case-insensitive substring match).
 *
 * Drives the search box in the Open lane's filter dropdown. A blank or
 * whitespace-only query returns `tags` unchanged (show everything); otherwise the
 * union is narrowed to options containing the query. Input order is preserved, so the
 * case-insensitive ordering from `availableTags` carries through.
 */
export function filterTags(tags: string[], query: string): string[] {
  const q = query.trim().toLowerCase();
  if (!q) return tags;
  return tags.filter((tag) => tag.toLowerCase().includes(q));
}

/**
 * Filter tasks by an inclusive-OR tag selection.
 *
 *  - Empty `selected` → return `tasks` unchanged (show all).
 *  - Non-empty → keep a task iff it carries at least one selected tag. Tasks with
 *    no tags are therefore excluded whenever a filter is active.
 */
export function filterTasksByTags(tasks: TaskWithDeps[], selected: string[]): TaskWithDeps[] {
  if (selected.length === 0) return tasks;
  const want = new Set(selected);
  return tasks.filter((t) => t.tags.some((tag) => want.has(tag)));
}

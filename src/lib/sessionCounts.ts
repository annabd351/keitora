// Pure helpers for deriving per-project session counts from daemon session
// hints. Extracted into their own module so they can be unit-tested without
// pulling in the full App.tsx React tree or the Tauri runtime.

/**
 * Group a flat list of session hints by project name, returning a count map.
 * Pure + side-effect-free so it can be used in both App.tsx and tests. Accepts
 * anything carrying a `project` field (e.g. `DaemonSessionHint[]`).
 *
 * Example:
 *   countsByProject([{project:"a"}, {project:"a"}, {project:"b"}])
 *   → Map { "a" → 2, "b" → 1 }
 */
export function countsByProject(sessions: readonly { project: string }[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const s of sessions) {
    m.set(s.project, (m.get(s.project) ?? 0) + 1);
  }
  return m;
}

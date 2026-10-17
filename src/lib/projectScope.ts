// Shared helpers for project-scoped live resources (sessions, tasks). When the
// active project switches, a resource's live data lags behind the (instant) visual
// switch; these helpers let a hook seed from a per-project cache until the active
// project's own data lands, without ever masking a genuine "loaded empty" state.

/**
 * Decide whether to seed a project-scoped view from its per-project cache.
 *
 * Seed ONLY before the active project's first real load has landed — i.e. the live
 * value for the active project is still empty AND we have not yet observed real data
 * for it. Once a real load has been seen (`loadedProject === activeProject`), an
 * empty live value is a *genuine* empty state and must be shown as empty, never
 * masked by stale cached data (the cache must not represent
 * "loaded empty").
 */
export function shouldSeedFromCache(
  filteredSize: number,
  loadedProject: string | null,
  activeProject: string | null,
): boolean {
  return !!activeProject && filteredSize === 0 && loadedProject !== activeProject;
}

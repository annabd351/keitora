// Single source of truth for the registered-project list. Wraps useLiveResource
// (listen-first, cancel-safe, latest-wins, periodic re-sync) so every consumer —
// the rail's project groups, the keymap's cycle order, etc. — reads one fetch + one
// subscription instead of each component fetching independently (which raced and
// could leave the rail groups and the keyboard cycle order transiently disagreeing).

import { listProjects, type ProjectSummary } from "./ipc";
import { listenAlso, useLiveResource } from "./useLiveResource";

// A reorder emits only `projects://reordered`, which nothing project-scoped reacts to.
const options = { listen: listenAlso(["projects://reordered"]) };

export function useProjectList(): ProjectSummary[] {
  // `listProjects` and `options` are stable module-level values, so the subscription
  // isn't torn down/recreated each render.
  const { data } = useLiveResource(listProjects, "project://changed", options);
  return data ?? [];
}

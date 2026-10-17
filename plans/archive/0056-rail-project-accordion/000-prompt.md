# Rail project accordion — implementation prompt

Entry point for implementing the rail project accordion. The design lives in the proposal;
these stage files only sequence the work and say how to validate each step.

- Proposal (source of truth): `plans/0056-rail-project-accordion.md` — read it fully, especially
  "Naming and keys", the File change map and Testing before starting any stage.
- Repo conventions: `AGENTS.md`.
- Branch: `llm/andrew/keitora/0056-rail-project-accordion` (already checked out; do not create another).

## Stages (execute in order)

| # | File | Goal |
|---|---|---|
| 1 | `001-backend-project-order.md` | `project_order` in `UserConfig`, `reorder_projects`, ordered `list_projects`, `ProjectSummary.resolved_name` |
| 2 | `002-backend-projects-overview.md` | `projects_overview` command + testable `project_overview` helper |
| 3 | `003-frontend-pure-libs.md` | IPC types/wrappers, overview hook, `buildGroups`, expanded reducer, JSON local storage, `laneState` move, pending-intent rules |
| 4 | `004-switch-project-plumbing.md` | key vs resolved-name fix, `handleSwitchProject` options, silent switch, pending-intent slot in App |
| 5 | `005-rail-lane-extraction.md` | Extract `Lane` verbatim into `rail/` with no visual change |
| 6 | `006-rail-project-groups-ui.md` | Project groups, collapsed summary, active header, focused lane, sticky headers, motion, rail top row |
| 7 | `007-reorder-and-menus.md` | Drag reorder, project context menu, non-active lane menu, Reveal |
| 8 | `008-topbar-cleanup-docs.md` | Slim top bar + breadcrumb, delete `ProjectTabs`, stale comments, build plan docs |

## Execution rules

- **Sequential.** Finish, validate and commit each stage before starting the next. Each stage is a
  coherent commit that leaves `tsc` and the relevant tests green on its own.
- **Tests ship with the logic, in the same commit.**
  - Rust logic gets `#[cfg(test)]` coverage in the same commit (AGENTS.md).
  - Frontend pure logic gets vitest in the same commit. The repo's vitest runs in a **node**
    environment with **no component rendering**, so keep logic in pure modules under `src/lib/`
    and keep `.tsx` components thin.
- **Commits:** plain Conventional Commits (`type(scope): description`). No task or plan identifiers
  (no `#NNNN`, no "Stage N") in commit messages or source. End each commit message with
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Before each commit: `git grep -nE '#[0-9]{3,4}' -- <changed source files>` must be empty, and
  `git log -1 --format=%B | grep -E '#[0-9]'` must be empty. `plans/` is exempt.
- Never compare a registry key to a resolved name; name variables `projectKey` / `resolvedName`.

## Machine-load constraints (the user's machine is busy and runs a real Keitora)

- **Never** run gated herdr tests (anything needing `KEITORA_HERDR_BIN`).
- **Never** run `dev:herdr`, `yarn tauri dev`, or the app itself.
- **Never** restart or kill herdr or keitora processes (not by image name, not at all).
- Prefer targeted runs: `cargo test -p <crate> <filter>` and `yarn test <file>` over repeated full
  runs. Run `yarn tsc --noEmit` for type checks. A single full `cargo test --workspace` and
  `yarn test` at the very end of the last stage is acceptable; avoid heavy repeats.
- Visual and runtime smoke checks listed in the proposal's Testing section are deferred to a
  later phase; stages here validate by type check and unit tests only.

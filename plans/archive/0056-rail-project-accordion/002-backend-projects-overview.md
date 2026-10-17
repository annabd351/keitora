# Stage 2 — Backend cross-project overview

## Goal

Add the `projects_overview` command returning worktrees and reconciled session states for every
registered project, so non-active lanes can show real status lights. Proposal section: §1.

## Prerequisites

- Stage 1 committed (`ordered_project_names`, `ProjectSummary.resolved_name`).

## Implementation

`src-tauri/src/host.rs`, next to `list_projects`:
- `#[derive(serde::Serialize)] pub struct ProjectOverview { key, resolved_name, worktrees: Vec<WorktreeView>, sessions: Vec<WorktreeSession> }`
  (wire shapes identical to the `worktrees` and `sessions_list` commands).
- Pure-ish helper (no Tauri `State`, mirrors `worktrees_in` taking `&dyn PtyBackend`):
  `fn project_overview(client: &dyn PtyBackend, key: &str, pc: keitora_tasks::ProjectConfig, live: &HashSet<SessionKey>, bg: &HashSet<SessionKey>) -> Result<ProjectOverview, String>`.
  Order matters because `worktrees_in` consumes `pc`:
  1. `let resolved = pc.project.name.clone();`
  2. sessions: only if `pc.tasks.db_path.exists()`, call `crate::tasks::reconciled_sessions(live, bg, &resolved, &pc)?`
     and stamp each row's `.project = resolved.clone()`; otherwise `Vec::new()`. Never create a DB
     file for a project that has none.
  3. `worktrees_in(client, Some(pc))?`.
- Command `#[tauri::command(async)] fn projects_overview(client: State<SharedBackend>) -> Result<Vec<ProjectOverview>, String>`:
  take `live = client.live_set_snapshot()` and
  `bg = crate::backend::cached_background_set(client.inner().as_ref())` once before the loop; iterate
  `user_cfg.ordered_project_names()`; `keitora_tasks::effective_config(pref)` per project; skip a
  project on config error or when the helper returns `Err`. Consider factoring the loop into a small
  function over `&UserConfig` so the skip behaviour is unit-testable (proposal Testing).
- Register `projects_overview,` directly after `list_projects,` in `generate_handler!`.
- No new events are emitted.
- While here, fix the stale doc comment at `src-tauri/src/activity/mod.rs:46` (the project field is
  the resolved name, not the registry key; see `activity/mod.rs:126-129`).

Tests (`host.rs` tests module), using `crate::backend::fake::FakeBackend::new()` and a
`tempfile::TempDir` file-less project through `effective_config(&ProjectRef::new(root))` (pattern of
`project_summary_from_fileless_effective_config`):
- `key` and `resolved_name` populated and distinct when the root basename differs from the key.
- Worktrees returned with `terminated` taken from the fake backend.
- Sessions empty and **no DB file created** when `db_path` does not exist.
- Sessions scoped to the project's configured worktrees and stamped with `resolved_name` when the DB exists.
- If the loop is factored: a broken-config project is skipped.
Note `resolved_worktrees` reads the real user config via `UserConfig::load()` read-only, as existing tests do.

## Validation

```
cargo test -p keitora project_overview
cargo test -p keitora overview
cargo check -p keitora
```
Targeted runs only. Never run gated herdr tests.

## Commit message

```
feat(projects): add cross-project overview command

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

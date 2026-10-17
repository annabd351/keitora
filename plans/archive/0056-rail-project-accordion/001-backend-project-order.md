# Stage 1 — Backend project order and resolved name

## Goal

Give the registry a persisted display order, expose a command to change it, make `list_projects`
follow it, and add `ProjectSummary.resolved_name` so the frontend can tell registry keys from
resolved project names. Proposal sections: "Naming and keys", §1 (resolved_name part), §5 (Backend).

## Prerequisites

- On branch `llm/andrew/keitora/0056-rail-project-accordion`, clean tree (untracked `claude/` is unrelated).
- Read `plans/0056-rail-project-accordion.md` and `AGENTS.md`.

## Implementation

`crates/keitora-tasks/src/config.rs`
- `UserConfig` (near `config.rs:234`): add
  `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub project_order: Vec<String>`
  declared immediately after `active`, before the `claude`/`codex`/... tables so TOML emits it as a
  plain value ahead of any table. `projects` stays a `BTreeMap`.
- `impl UserConfig` next to `set_active` (`config.rs:915`):
  - `pub fn ordered_project_names(&self) -> Vec<String>`: `project_order` entries that exist in
    `projects` (first occurrence wins, duplicates and stale entries ignored), then remaining keys
    alphabetically. Empty order gives pure alphabetical.
  - `pub fn set_project_order(&mut self, order: Vec<String>) -> Result<(), String>`: the order must
    be a permutation of current keys (same length, no duplicates, none unknown, none missing);
    otherwise `Err` naming the problem; on success store it.
- Tests in the existing config tests module: empty order is alphabetical; stale entries ignored;
  newly registered project appended after the explicit order; duplicates ignored;
  `set_project_order` accepts a permutation and rejects unknown, missing and duplicate keys; TOML
  round-trip (`save_to` / `load_from`) with non-empty `project_order` and `[projects]` proving the
  serialized ordering is valid; empty `project_order` is not serialized.

`src-tauri/src/host.rs`
- `ProjectSummary` (`host.rs:78`): add `pub resolved_name: String`. `project_summary()` (`host.rs:96`)
  sets `resolved_name = pc.project.name.clone()` and keeps `name = pc.project.name`.
- `list_projects` (`host.rs:121`): iterate `user_cfg.ordered_project_names()` instead of
  `&user_cfg.projects`; keep overwriting `summary.name = key` but leave `resolved_name` untouched.
- New command `reorder_projects(app: AppHandle, order: Vec<String>) -> Result<(), String>`
  (`#[tauri::command(async)]`): `UserConfig::load()`, `set_project_order(order)`, `save()`,
  `app.emit("project://changed", ())`. Register `reorder_projects,` directly after
  `set_active_project,` in `generate_handler!`.
- Extend `project_summary_maps_fields` to assert `resolved_name`. If `list_projects`' ordering can
  be factored into a tiny pure function, test it; otherwise the config tests cover ordering.
- Fix any other `ProjectSummary { .. }` constructions the compiler flags.

## Validation

```
cargo test -p keitora-tasks config
cargo test -p keitora project_summary
cargo check -p keitora
```
(Use the actual `src-tauri` package name from `src-tauri/Cargo.toml` for the last two; do not run
gated herdr tests or the whole workspace.)

## Commit message

```
feat(projects): persist project order and expose resolved name

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

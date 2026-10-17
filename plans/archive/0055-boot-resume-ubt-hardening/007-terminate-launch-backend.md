# 007 - Terminate / launch backend

## Goal
Let a session be terminated so it stays down across restarts, and launched again, enforced by recovery, spawn and the IPC layer.

## Prerequisites
001 (store methods) and 003 (resume runner, `resume_locked` shape) committed; 006 committed for strict order.

## Implementation
Proposal C1 (ResumeStore wrapper), C2, C3. Paths under `src-tauri/src/`.

Order:
1. `backend/herdr/resume.rs`: `ResumeStore::{set_terminated, is_terminated, terminated_in, skip_resume}` (`set_terminated` returns `Err` when there is no DB).
2. C2: replace `agent_quit` with `skip_resume` at exactly the four sites (`note_server_death`, `mark_owed`, two in `reconcile_lineage` in `reconcile.rs`); add the backstop at the top of `Spawner::resume_locked` (resolve recovered, return `Ok(None)`).
3. C3 backend: `PtyBackend::{terminated_keys, set_terminated}` for `HerdrBackend`, `UnavailableBackend`, `FakeBackend`; `PtyBackendError::Terminated` + `Display`; `HerdrBackend::spawn`/`spawn_agent` refuse a terminated key right after `admit(None)`.
4. `host.rs`: `TERMINATED_TOKEN`, `spawn_err_kind`/`attach_err_kind` mapping (update their tests); `session_terminate` and `session_launch` commands plus `generate_handler!` registration; `WorktreeView` and the `worktrees()` change.
5. `transfer.rs`: refusal before `with_recovery_lock`. `dispatch.rs`: terminated reason/detail texts in the `Queued` and `NoSuchSession` paths.

## Validation
```
cargo check -p keitora
cargo clippy -p keitora
cargo test -p keitora resume
cargo test -p keitora reconcile
cargo test -p keitora host
cargo test -p keitora transfer
cargo test -p keitora dispatch
```
Use `FakeBackend` and mocked seams only; no real herdr, no gated tests.

Required tests: `skip_resume` at all four sites; the `resume_locked` backstop; `spawn`/`spawn_agent` refuse a terminated key; `ResumeStore::set_terminated` is `Err` with no DB; launch clears the flag and spawns; terminate during recovery still sets the flag and succeeds; `session_terminate` with `gen = None` resolves the generation from `cached_sessions`, skips the kill for a non-live key, and succeeds when the kill is refused with `Recovering`; `attach_err_kind_tokens`/`spawn_err_kind_tokens` include `Terminated`; the transfer refusal leaves the registry unchanged; `worktrees()` carries `terminated` (and still returns empty with no active project); `dispatch_apply` reports "terminated" in both the `Queued` and `NoSuchSession` paths.

## Commit message
```
feat(sessions): add terminate and launch for sessions

Persist a terminated flag so a session stays down across restarts and
recovery, add session_terminate and session_launch commands, refuse to
spawn, transfer or dispatch to a terminated session, and expose the flag on
the worktree list.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

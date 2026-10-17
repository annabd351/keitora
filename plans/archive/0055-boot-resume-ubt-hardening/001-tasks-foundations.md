# 001 - keitora-tasks foundations

## Goal
Land the two persisted/config pieces in `crates/keitora-tasks` that later stages consume: the `resume_concurrency` setting and the `session_flags` terminated-flag storage.

## Prerequisites
None (first stage). Branch checked out, tree clean.

## Implementation
- Proposal A2, "Config": add `RecoveryConfig.resume_concurrency: u32` (default 3, doc comment), clamp `< 1` to 1 in `normalized()`. Update every literal/test the proposal lists in `crates/keitora-tasks/src/config.rs` (defaults test, serde round-trip, the two `normalized` tests). Do NOT touch `RecoveryParams` here (machine stage).
- Proposal C1: `session_flags` table created with `CREATE TABLE IF NOT EXISTS` in `init_schema` (`crates/keitora-tasks/src/sqlite_backend.rs`) beside `resume_sessions`; `LocalStore::{set_terminated, is_terminated, terminated_in}` in `local_store.rs` with the exact signatures given. Do not touch `ResumeStore` yet.
- Order: config first, then table + methods. Tests in the same files.

## Validation
```
cargo check -p keitora-tasks
cargo clippy -p keitora-tasks
cargo test -p keitora-tasks config
cargo test -p keitora-tasks terminated
```
Also `cargo check -p keitora` to confirm no other crate literal-constructs `RecoveryConfig` with every field (fix if so).

Required tests: `resume_concurrency` default is 3; serde round-trip; clamp to >= 1 (0 becomes 1, large values untouched); `session_flags` set, clear and query, including a key with no resume row; no row = `false`; `terminated_in` ordered by name and scoped to the project; repeated set is idempotent.

## Commit message
```
feat(tasks): add resume concurrency setting and terminated session flags

Add a resume_concurrency recovery option (default 3, clamped to at least 1)
and a session_flags table with LocalStore accessors to persist whether a
session was terminated by the user.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

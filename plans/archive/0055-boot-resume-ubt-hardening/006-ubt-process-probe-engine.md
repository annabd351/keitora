# 006 - UBT process probe, engine rules, staleness

## Goal
Stop stuck and misattributed build bars by combining log content with live process evidence, a run-consistency guard and run-scoped staleness.

## Prerequisites
005 committed.

## Implementation
Proposal B2, B3, B4, in `src-tauri/src/activity/progress/` (`procs.rs` new, `ubt.rs`, `mod.rs`). `ActivityState` in `activity/mod.rs` keeps calling `UbtEngine::tick(&wt_paths, now_ms)` unchanged.

Order:
1. `ubt.rs`: `extract_project_from_args`, `is_build_mode` (+ tests: paths with spaces, quoted value, case-insensitive, the `-Mode=` rule).
2. `procs.rs` (declare `pub mod procs;`): `UbtProc`, `ProbeSnapshot`, `UbtProbe`, `SysinfoProbe` (name-first refresh, command-line refresh for candidates only, 2 s cache with stable `seq`, reuse the scan pattern from `backend/herdr/real.rs`), plus a test fake. Real process scanning stays out of unit tests (fakes and pure helpers only).
3. `mod.rs` state and wiring: `Run.dead`, `Run.last_line_ms`; `Inner.pending/streak/last_probe_seq`; `PendingAction`; `UbtEngine::new(log_path, probe)` and `Default` using `SysinfoProbe`; update the six existing `UbtEngine::new(Some(..))` test calls with an "unavailable" fake; refresh the module doc.
4. Rules 1-5 with the streak (new snapshots only), cancel/reattribute application, dead-run behaviour, probe-call gating (attributed running run, or log mtime within 30 s).
5. B3 consistency guard in `Inner::apply`.
6. B4: `surface_decision(finished, has_progress, run_age_ms, process_confirmed)`, `UNCONFIRMED_STALE_AFTER_MS = 90_000`, `Inner::surface(now_ms, run_age_ms, process_confirmed)`, `process_confirmed` tracking, `run_age_ms = max(...)` in `tick`.
7. Fixtures (generic names only): `ubt_cancelled.log` (no `Result:`) and a two-run rotation pair, beside the existing UBT test fixtures.

## Validation
```
cargo check -p keitora
cargo clippy -p keitora
cargo test -p keitora progress
cargo test -p keitora activity
```
Required tests (proposal "Progress engine" list): cancelled run (no `Result:`, process gone for 2 new snapshots) clears the bar and `working`; late `Result:` wins; A cancelled then B starts with a longer rotated log shows B only; B's steps with a missed header are not credited to A; a `total` change mid-run starts a new unattributed run; an unknown live UBT keeps the run and never reattributes; `-Mode=QueryTargets` ignored, `-Mode=Build` and no `-Mode=` count; a dropped run is not resurrected by later lines; streak needs 2 new snapshots (same `seq` twice does not count); an unavailable probe never cancels and keeps the 5-minute window; exactly one other worktree live for 2 snapshots reattributes, two other worktrees drops without reattributing, an unknown UBT blocks both; an unattributed run is attributed only when exactly one build UBT maps and none is unknown; `surface_decision` 90 s vs 5 min and a long-finished log at startup shows nothing.

## Commit message
```
fix(activity): confirm build activity with live processes

Probe running UnrealBuildTool processes to drop cancelled builds,
reattribute runs to the worktree actually building, reject step lines that
belong to a different build, and scope staleness to the run instead of the
shared log file.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

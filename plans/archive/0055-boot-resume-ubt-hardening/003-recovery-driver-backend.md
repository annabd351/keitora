# 003 - Recovery driver, gate and backend wiring

## Goal
Run resumes concurrently on worker threads, make the gate and ordinary callers key-aware so settled sessions unlock, classify load-related outcomes, and time herdr CLI calls.

## Prerequisites
002 committed.

## Implementation
All under `src-tauri/src/backend/herdr/`. Implements the remaining parts of proposal A1 (Gate, caller table), A2 (Driver, per-key locks) and A3 (load-related rule), plus A5 CLI timing.

Order:
1. A3 plumbing first: `Spawner::spawn_agent_locked` returns `(u64, ResumeResult, bool)` (`fell_back`); `resume_locked` returns `Result<Option<(ResumeResult, bool)>, _>`; adjust `spawn_agent_with`, `Spawner::resume_session`, `HerdrBackend::spawn_agent`, `RecoveryParts::resume_session` (non-runner callers ignore the flag).
2. Driver (A2 "Driver - exact shape"): `ResumeRunner` / `ResumeReport`; `Executor::resume_runner` replaces `Executor::resume`; implement for `BackendExecutor` (new `BackendResumeRunner` with the generation re-check before `store.clear` and any herdr call, and the load-related classification rule), `FakeExecutor`, `Idle`. `ResumeDispatch { Threads, Inline }`, `Driver::new(.., tx, dispatch)`, `Parked` keeps a `tx`; `Threads` spawns a `herdr-resume` thread with `catch_unwind` (panic -> `Failed("resume panicked")`, not load-related); `Inline` for test rigs.
3. Driver lifetime: `RecoveryHandle::shutdown()`, the driver loop stops on `Event::Shutdown`, and `impl Drop for HerdrBackend` calls it.
4. Gate (A1 "Gate" and caller table): `ServerGate::admit_session` with default body `self.admit(Some(key))`, `RecoveryGate` override (Healthy ok; Paused refused; Resuming ok for in-flight or settled; else `RecoveryPending`); switch exactly the callers in the table (`WriterIo::admit`, `WriterIo::write_line`, `attach`, `repaint`, `write`, `write_line`, `resize`); leave the others keyless. Update `set_view` callers in tests.
5. A5 timing: in `real.rs`, time `run_with_deadline` in `RealHerdr::run` and `run_raw`; `log::debug!("herdr cli {} took {} ms", ..)` with at most the first two args; log on success and error.

## Validation
```
cargo check -p keitora
cargo clippy -p keitora
cargo test -p keitora recovery
cargo test -p keitora gate
cargo test -p keitora driver
cargo test -p keitora resume
cargo test -p keitora real
```
Use only mocked/fake seams; skip anything gated on `KEITORA_HERDR_BIN`.

Required tests: `ResumeDispatch::Threads` sends `ResumeResult` from a worker; a worker whose generation moved on sends nothing (runner returns `None` before touching the store or herdr); `Event::Shutdown` stops the driver; a panicking worker reports `Failed`; `load_related` classification (timeout error code/text and `HerdrError::Timeout` true; early-exit fallback true; stored `Fresh` for a missing transcript, `NoSuchSession` and `AlreadyRunning` false); gate: `admit_session` refused while `Paused`, settled admitted only in `Resuming`, queued/failed/keyless refused, default trait body equals `admit(Some(key))`; `BootRecovery { owed }` is non-zero only for `RecoverDown`; `gate_cell_published_before_commands_run` still passes; the CLI timing subcommand-token helper (if extracted) logs at most two tokens.

## Commit message
```
feat(herdr): run resumes concurrently and unlock settled sessions

Execute resume commands on short-lived worker threads guarded by a
generation check, report load-related outcomes to the recovery machine,
admit settled sessions through the server gate, and log herdr CLI call
durations.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

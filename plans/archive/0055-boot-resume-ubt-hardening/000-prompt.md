# Implementation entry point

Proposal (source of truth, fully refined): `plans/0055-boot-resume-ubt-hardening.md`.
Read it and `AGENTS.md` before starting. Stage files below say which proposal
sections/sub-bullets they implement; they deliberately do not repeat the proposal.

Branch: `llm/andrew/keitora/0055-boot-resume-ubt-hardening` (already checked out).

## Stages (strictly in this order)

1. `001-tasks-foundations.md` - `resume_concurrency` config, `session_flags` table and `LocalStore` methods.
2. `002-recovery-machine.md` - pure recovery machine: in-flight set + cap, settled, load-related cap drop and stagger, `passes(n)`, A4 boot skip, timing plumbing; gate view and payload shape.
3. `003-recovery-driver-backend.md` - `ResumeRunner` / worker threads / shutdown, `admit_session` and key-aware callers, `load_related` classification, CLI call timing.
4. `004-frontend-recovery.md` - `RecoveryInfo.settled` and `paneRecoveryState`.
5. `005-ubt-tailer-fingerprint.md` - tailer content-identity rotation detection (B1).
6. `006-ubt-process-probe-engine.md` - process probe, engine rules, consistency guard, run-scoped staleness, fixtures (B2-B4).
7. `007-terminate-launch-backend.md` - terminate/launch backend (C1 store wiring, C2, C3).
8. `008-terminate-launch-frontend.md` - terminate/launch frontend (C4).

Implementation is SEQUENTIAL: finish and validate a stage, commit it, then start the next.
Never start a stage while the previous one fails validation.

## Test rigor

- New Rust logic ships with `#[cfg(test)]` tests in the same commit as the logic.
- Frontend logic ships with vitest tests in the same commit.
- The proposal's "Tests" section lists the required cases; each stage's Validation names the ones it owns.
- Update every existing test/literal broken by a signature or struct change in the same stage so each commit compiles and passes.

## HARD CONSTRAINT: never disturb a real herdr

- NEVER start, stop, restart or kill a real herdr process. No gated `KEITORA_HERDR_BIN` tests. Do not run the app (`yarn dev*`, `tauri dev`, etc.).
- The machine is loaded. Per-stage validation uses only:
  - Rust: `cargo check -p <crate>`, `cargo clippy -p <crate>`, and targeted `cargo test -p <crate> <filter>`.
  - Frontend: `yarn vitest run <file>` and `yarn tsc --noEmit`.
- Crates: `keitora-tasks` (`crates/keitora-tasks`) and the Tauri crate (`src-tauri`, package `keitora`).
- Full `cargo test --workspace`, `yarn test`, `yarn build` and biome run ONCE, at the later Testing stage, not per stage.
- Tests exercise recovery only through the pure `Sim`, the driver `FakeExecutor`/`Rig`, and mocked seams. Nothing may spawn a real `herdr`.

## AGENTS.md rules

- No plan/task identifiers anywhere in code, comments, test names, fixtures or commit messages: no `#NNNN`, no "Stage N", no `plans/...` paths. Explain what/why on its own terms.
- Commit messages: Conventional Commits, `type(scope): description`, no trailing identifiers.
- Commit trailer, exactly: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
- Before each commit: `git grep -nE '#[0-9]{3,4}' -- <changed files>` must be empty, and `git log -1 --format=%B | grep -E '#[0-9]'` must be empty.
- The repo is open source: fixtures and tests use generic names only (`proj`, `MyGame`, `alpha`/`beta` worktrees). No real project, user or machine names.
- Do not edit files under `plans/` during implementation.

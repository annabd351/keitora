# 002 - Recovery machine (pure)

## Goal
Make the pure recovery state machine support several concurrent resumes, progressive unlock (`settled`), load-sensitive cap and stagger, headroom scaling, boot-time sustain skip, and timing emission. Everything stays deterministic and tested through `Sim`; no threads yet.

## Prerequisites
001 committed (`RecoveryConfig.resume_concurrency` exists).

## Implementation
All under `src-tauri/src/backend/herdr/` (`recovery/machine.rs` unless noted). Implements proposal A1 (machine, gate view, payload), A2 (config plumbing, machine, `passes`, failure paths, harness), A3 (machine side), A4, A5 (machine plumbing + lifecycle event).

Because the machine owns the types the driver and gate consume, this stage also makes the minimum shape changes elsewhere so the crate compiles:
1. `RecoveryParams.resume_concurrency` from the normalized config (A2 Config, machine bullet).
2. `inflight: HashSet<SessionKey>` and `resume_cap`, following the proposal's per-function table exactly; accept `Event::ResumeResult` in every phase (A2 Event handling); `pump` loop, `resume_gate_sample`, failure paths and crash accounting (A2).
3. `passes(p, snap, sessions)` with both required_* scaled (A2 `passes`); update all call sites and the test-harness predicate.
4. `settled` set + accessor and its add/remove/clear rules (A1 Machine).
5. `Event::ResumeResult` gains `load_related`; the machine applies A3 (cap drops to 1 on the first load-related failure; stagger after every `Failed` and any `load_related` outcome; none after a clean outcome). The existing driver executor passes `load_related: false` for now (classification arrives in 003).
6. `Event::BootRecovery { owed }`, `Episode.boot_owed` / `boot_sustain_checked`, and the A4 rule in `evaluate_sample`. `RecoveryHandle::boot_recovery` gains the `owed` parameter; update its four call sites in `RecoveryParts::boot` here so everything compiles (`RecoverDown` passes `self.keymap.should_be_running().len() as u32` right after `mark_owed()`, the other three pass 0).
7. `Event::Shutdown` variant added and ignored by the machine (driver handling is in 003).
8. Gate/view shape (needed because the machine's `inflight()` is now a set): `GateView { phase, inflight: HashSet, settled: HashSet }`, `RecoveryGate::set_view(phase, inflight, settled)`, `RecoveryGate::admit` `Resuming` arm uses `key in inflight`; driver `publish` fills both sets. Do NOT add `admit_session` or change callers yet.
9. `RecoveryPayload.settled: Vec<SessionRef>` (sorted), `healthy()` empty, filled in `wire(seq)`; update `gate.rs` test `payload_json_has_every_field_with_nulls`.
10. A5 machine plumbing: `Command::RecordTiming`, `Episode` timing fields, emission only on the `Resuming -> Healthy` finish; `LifecycleEvent::RecoveryTiming` + `format_event` in `src-tauri/src/lifecycle.rs`; driver `execute` maps the command to `lifecycle::record` and returns `None`. (The `real.rs` CLI timing log is in 003.)
11. Test harness/Sim updates listed in A2 "Test harness": invariants use `resume_cap` and `inflight.len()`, update the named tests; set `resume_concurrency = 1` in existing tests that need serial behaviour; rewrite `stagger_five_seconds_between_resumes` for A3.

Order inside the stage: params/config -> inflight set + cap + pump -> passes -> settled -> A3 -> A4 -> timing -> gate view/payload -> harness and new tests. The crate must compile and pass at the end (intermediate states may not).

## Validation
```
cargo check -p keitora
cargo clippy -p keitora
cargo test -p keitora recovery
cargo test -p keitora lifecycle
cargo test -p keitora gate
```
Required tests (machine/gate/payload/lifecycle items from the proposal's Tests section): cap respected and queue order preserved; `passes(n)` scales commit and physical requirements (n = 1/2/3); no stagger after a clean resume; stagger after every `Failed` and after a `load_related` outcome only; first timeout/early-exit drops cap to 1 and a non-load failure does not; memory-sample failure with work in flight drains before `AwaitHeadroom`; stale-generation result ignored; `pause` requeues all in-flight; `cancel_key` on one of several in-flight; `finish` waits for empty in-flight; `settled` rules (adds incl. `AlreadyRunning`, not `Failed`; cleared by death/boot/user-restart/explicit-stop, not `finish`; loses a cancelled key); death with 3 in flight (one breaker death, `halve_budget` counts 3, all requeued, late old-generation results discarded); A4 (passes for N skips sustain; passes for 1 but not N keeps it; non-boot keeps it; only the first sample of the first wait); `RecoveryTiming` formatting and a single `RecordTiming` on `Resuming -> Healthy`; payload `settled` JSON; gate view admits in-flight keys in `Resuming` and refuses others; `BootRecovery { owed }`.

## Commit message
```
feat(herdr): resume several sessions at once in the recovery machine

Track a set of in-flight resumes bounded by a configurable cap, unlock
sessions as they settle, stagger only after failures or load-related
outcomes, scale the headroom requirement by the number of concurrent
sessions, skip the boot sustain when headroom covers the whole queue, and
record recovery timing.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

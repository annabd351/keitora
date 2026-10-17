# 0055 — Faster boot resume, hardened UBT activity, session terminate/launch

**Task:** #55 "herdr boot resume speed + UBT activity hardening"
**Branch:** `llm/andrew/keitora/0055-boot-resume-ubt-hardening` (based on the open-source `origin/main`)
**Status:** proposal: refined (cycle 2, automated review applied)
**Complexity:** medium–large. Three independent areas, about 25 files.

This task covers two bugs and one feature, tracked as one task at the user's request:

- **A. Boot resume is slow.** After a reboot it takes minutes before any pane is usable.
- **B. UBT activity is fragile.** A cancelled build leaves the rail stuck, and one worktree's build can show up on another worktree's lane.
- **C. Terminate / Launch.** Right-click a session to terminate it so it stays down across restarts; right-click again to launch it. This also shrinks A's boot queue.

---

## Hard constraint: never disturb the primary herdr

The user runs a primary herdr daemon with critical live sessions on the development machine. Nothing in this work may stop, restart or kill it.

- All recovery logic is tested **only** through the pure state machine (`Sim`), the driver's `FakeExecutor`/`Rig`, and the mocked herdr seams. No test spawns a real `herdr`. No gated (`KEITORA_HERDR_BIN`) tests are run.
- A dev Keitora pointed at the real herdr would run exactly this boot-recovery path, which may `stop_and_start` the server. The app is therefore only ever smoke-tested through `yarn dev:isolated` (`scripts/dev-herdr.mjs`). That script gives the app its own `KEITORA_HOME`, so it gets its own herdr state and its own `resume.db`, and it never shares the user's terminated flags.
- The app is only smoke-tested with the user's explicit go-ahead, and not while the machine is under heavy load.
- The reboot scenario (A) is verified from `keitora.log` timestamps on the user's next real reboot. It is not simulated here.
- `cargo test --workspace` is run **once** per verification pass, not in loops.

---

## A. Boot resume speed

### Evidence (`~/.keitora/logs/keitora.log`, launch on 2026-10-12)

| Phase | Wall time |
|---|---|
| `down → await_headroom → restarting` (memory sustain) | **47 s** |
| herdr start + reconcile | 3 s |
| `resuming → healthy`: 24 sessions resumed one at a time | **4 m 00 s** |
| total launch → usable | **≈ 4 m 50 s** |

- Each clean resume takes about 4.3 s, followed by a fixed 5 s stagger, so each session costs about 9.3 s.
- Three sessions failed with herdr's `timed out waiting for agent startup`, at about 15.6 s each.
- Every pane is gated (`recovering`) until the whole queue finishes.

### Where the serialization lives

It is deliberate, a leftover of the daemon-reliability work. It is **not** in the transport (each herdr call is its own CLI process) or in the frontend (panes attach independently).

| Point | Location |
|---|---|
| One resume in flight (`inflight: Option<SessionKey>`) | `src-tauri/src/backend/herdr/recovery/machine.rs` (`pump`, `start_next`, `resume_gate_sample`, `resume_result`) |
| 5 s stagger after every resume | `machine.rs` `resume_result` → `stagger_until`; `RecoveryConfig::stagger_secs` (`crates/keitora-tasks/src/config.rs`) |
| Driver executes commands synchronously, inline | `recovery/driver.rs` (`Executor: Send`, `&mut self`, `execute` returns the result inline) |
| Gate admits only the in-flight key; every ordinary caller passes `None` | `recovery/mod.rs` `GateView { phase, inflight }` / `RecoveryGate::admit` (trait `ServerGate` in `herdr/gate.rs`); callers in `herdr/mod.rs` (`WriterIo::admit`, `WriterIo::write_line`, `HerdrBackend::{spawn, spawn_agent, attach, repaint, write, write_line, resize, kill, list_sessions, cached_sessions, exit_authority}`), `status_loop.rs` (the status loop's per-tick gate check) |
| Memory gate sized for one session | `machine.rs` `passes()` → `headroom.rs` `required_bytes(…, 1)` and `required_phys_bytes(…, 1)` |
| 60 s sustain before herdr even starts | `machine.rs` `enter_await_headroom` / `evaluate_sample` (`self.p.sustain`); `headroom_sustain_secs` |
| Frontend marks panes `recovering` while phase ≠ Healthy | `src/lib/recovery.ts` `paneRecoveryState`, `src/lib/paneStatus.ts` |

### Design

**Conventions used below (singletonized).** All Rust paths are under `src-tauri/src/backend/herdr/` unless they start with `crates/` or `src/`. "Machine" is `recovery/machine.rs`, "driver" is `recovery/driver.rs`, "gate module" is `gate.rs`, "recovery mod" is `recovery/mod.rs`, "backend mod" is `mod.rs`. Where this plan says "episode", it means the span from a user-visible recovery start (`process_boot`, `process_death`, `process_user_restart`) to `finish` reaching `Healthy`. Test names are given where they exist today.

**A1. Progressive availability: sessions unlock as their resume succeeds.**

- **Machine.** New field `settled: HashSet<SessionKey>`, the keys whose resume **succeeded** in this episode.
  - **Added** in `resume_result`, for a non-`cancelled` entry whose outcome is `Resumed`, `Continued`, `StartedFresh` **or** `AlreadyRunning` (the pane is live in all four). **Not** added for `Failed`. Never-owed, queued, suspended and failed keys are not in it.
  - **Removed** by `cancel_key` (for that key; `Event::CancelProject` already loops over `cancel_key`).
  - **Cleared** (`settled.clear()`) in `process_death`, `process_boot`, `process_user_restart` and `explicit_stop` (each starts a new episode or drops everything). **Not** cleared in `finish`: a later `healthy_reset` run (`Healthy → Resuming` for budget-suspended leftovers) must keep already-resumed sessions unlocked. (singletonized)
  - New accessor `pub fn settled(&self) -> &HashSet<SessionKey>`.
- **Gate view.** `GateView` (recovery mod) becomes `{ phase: Phase, inflight: HashSet<SessionKey>, settled: HashSet<SessionKey> }`. `Default` has both sets empty. The test helper `RecoveryGate::set_view` becomes `set_view(phase, inflight: HashSet<SessionKey>, settled: HashSet<SessionKey>)`; update its callers in recovery mod, driver and backend-mod tests. The driver's `publish` fills both sets from `machine.inflight()` / `machine.settled()` (clones).
- **Payload.** `RecoveryPayload` (`recovery/payload.rs`) gains `pub settled: Vec<SessionRef>`, where `#[derive(Clone, Debug, PartialEq, Eq, Serialize)] pub struct SessionRef { pub project: String, pub worktree: String }`, sorted by `(project, worktree)` so snapshots are deterministic. `RecoveryPayload::healthy()` sets `settled: vec![]`. The machine's `wire(seq)` fills it. Update `gate.rs` test `payload_json_has_every_field_with_nulls` (new key `"settled": []`).
  - Existing `queue` still carries `queued|resuming|suspended`.
  - TypeScript (`src/lib/ipc.ts`): `export interface RecoverySessionRef { project: string; worktree: string }`; `RecoveryInfo` gains `settled: RecoverySessionRef[]`; `HEALTHY_RECOVERY` gets `settled: []`. Fix every `RecoveryInfo` literal in the vitest files (`recovery.test.ts` and any other test that builds one).
- **Gate.** Trait `ServerGate` (gate module) gains `fn admit_session(&self, key: &SessionKey) -> Result<(), GateRefusal>` with a **default body** `self.admit(Some(key))`, so other implementors (`TestGate` in `status_loop.rs` tests) keep compiling. `RecoveryGate` overrides it: `Healthy` → ok; `Paused` → `GateRefusal::Paused` (settled keys are **not** admitted while paused); `Resuming` and (`key ∈ inflight` or `key ∈ settled`) → ok; anything else → `RecoveryPending`. Admitting in-flight keys is **existing behaviour**, not a widening: today's gate admits `key == inflight` so that recovery's own resume path (`resume_session` → spawn/attach for that key) can run. The only change is set membership. The frontend still shows in-flight panes as `recovering` and does not attach them until they are `settled`. `RecoveryGate::admit(Option<&SessionKey>)` keeps its contract except that the `Resuming` arm becomes `key ∈ inflight` (set membership). Settled keys are admitted **only** in `Resuming`, matching the frontend rule below. (singletonized)
- **Which callers become key-aware (`admit_session(key)`) and which stay keyless (`admit(None)`):**

  | Caller | Location | After |
  |---|---|---|
  | `WriterIo::admit` | backend mod, `impl writer::SessionIo for WriterIo` (uses `self.key`) | `admit_session(&self.key)` |
  | `WriterIo::write_line` | backend mod, same impl | `admit_session(&self.key)` |
  | `HerdrBackend::attach` | backend mod, `impl PtyBackend` | `admit_session(&key)` |
  | `HerdrBackend::repaint` | same | `admit_session(key)` |
  | `HerdrBackend::write` | same | `admit_session(key)` |
  | `HerdrBackend::write_line` | same | `admit_session(key)` |
  | `HerdrBackend::resize` | same | `admit_session(key)` |
  | `HerdrBackend::spawn` | same | stays `admit(None)` (spawn of a new key) |
  | `HerdrBackend::spawn_agent` | same | stays `admit(None)` |
  | `HerdrBackend::kill` | same | stays `admit(None)` (a terminate during recovery is refused with `Recovering`, see C3) |
  | `HerdrBackend::list_sessions`, `cached_sessions` | same | stay `admit(None)` |
  | `HerdrBackend::exit_authority` closure | backend mod | stays `admit(None)` |
  | status loop tick | `status_loop.rs` (`self.deps.gate.admit(None)`) | stays `admit(None)` |
  | `RecoveryParts::resume_session` | backend mod (`self.gate.admit(Some(key))`) | unchanged: the driver's own resume uses `admit(Some(key))` (in-flight only) |
  | driver test executor | `driver.rs` `FakeExecutor::resume` (`gate.admit(Some(key))`) | unchanged |

  So the rail's busy/idle lights for settled sessions refresh only at `Healthy`. This is accepted.
- **Frontend (`src/lib/recovery.ts`).** `paneRecoveryState(info, project, worktree)` keeps its first line (a `suspended` queue item wins). Then: if `info.phase === "Resuming"` **and** `info.settled.some(s => s.project === project && s.worktree === worktree)` return `null`; otherwise `info.phase !== "Healthy" ? "recovering" : null` as today.
  - Every other key keeps today's `recovering` state, including failed keys, so a failed pane stays parked showing its failure instead of looping attach → refuse → re-attach.
  - Every other phase is unchanged (`recovery.test.ts` keeps asserting this).
  - `src/lib/paneStatus.ts` needs no change for A1 (a `null` recovery state already re-attaches a waiting pane when the daemon is `Connected`).

**A2. Bounded concurrent resumes.**

- **Config.** `crates/keitora-tasks/src/config.rs`: new field `pub resume_concurrency: u32` on `RecoveryConfig` (doc: "How many sessions may resume at the same time."), default **3** in `impl Default for RecoveryConfig`.
  - Struct-level `#[serde(default)]` means it only needs a value in `Default`.
  - `normalized()` clamps `resume_concurrency < 1` to 1; no upper clamp.
  - `RecoveryParams` (machine) gains `pub resume_concurrency: u32` ("At least 1"), set in `RecoveryParams::from_config` from the normalized config.
  - Update: the config defaults test (asserts `stagger_secs == 5`), the serde round-trip test that builds `expected = RecoveryConfig { .. }`, the two `normalized` tests that build `RecoveryConfig { .. }` literals, and the literal in the `RecoveryParams::from_config` machine test (`stagger_secs: 2`). Any literal that names every field needs the new field.
- **Machine.** Field `inflight: Option<SessionKey>` becomes `inflight: HashSet<SessionKey>`; accessor `pub fn inflight(&self) -> &HashSet<SessionKey>` (replaces the `Option` one). New field `resume_cap: u32`, the effective cap, initialised in `new` to `p.resume_concurrency` and reset to `p.resume_concurrency` in `finish`, `explicit_stop`, `process_user_restart` and `process_boot`. A3 lowers it. It is **not** reset by `process_death` (a death inside an episode keeps the load evidence).
  - **Every `inflight` assignment site in `machine.rs`, by enclosing function:**

    | Function | Today | New |
    |---|---|---|
    | `RecoveryMachine` struct + `new` | `None` | `HashSet::new()` |
    | `process_death` | `= None` | `.clear()` |
    | `build_queue` | `= None` | `.clear()` |
    | `pump` (guard) | `if … inflight.is_some() { return }` | `if inflight.len() >= resume_cap { return }` |
    | `start_next` | `= Some(key)` | `.insert(key)` |
    | `resume_gate_sample` (guard) | `if inflight.is_some() { return }` | cap check, see below |
    | `resume_result` | `!= Some(&key)` then `= None` | `!inflight.contains(&key)` then `.remove(&key)` |
    | `finish` | `= None` | `debug_assert!(inflight.is_empty())` then `.clear()` |
    | `pause` | `inflight.take()` requeues one entry | `inflight.drain()`: every key's entry goes back to `Queued` |
    | `process_user_restart` | `= None` | `.clear()` |
    | `explicit_stop` | `= None` | `.clear()` |
    | `inflight()` accessor | `Option` | `&HashSet` |

    `halve_budget` does not touch the field (it counts `EntryState::Resuming` entries) and needs no change.
  - **Event handling.** In `handle`, the arm `Event::ResumeResult { .. } if self.phase == Phase::Resuming` loses its phase guard: it is accepted in **every** phase and `resume_result` itself discards a result whose generation differs from `self.generation` or whose key is not in `inflight`. (After `pause` or a death the in-flight set is already empty, so a late result is discarded; a session a late worker did bring up is later treated as already running by `resume_locked`.)
  - **`pump`.** Starts entries in a loop while `inflight.len() < resume_cap` and a queued, un-locked entry exists (`pick()`), and the stagger has passed. For each non-forced, non-bypassed start it needs the latest sample to pass `passes(p, snap, inflight.len() + 1)`. Forced entries skip the sample and the budget but still obey the cap and the stagger. The "done" branch (`pick()` is `None` and no `Queued` entry remains) and the budget-exhausted branch (park the rest as `Suspended(Budget)`) call `finish` **only when `inflight.is_empty()`**; otherwise they return and the last `resume_result` → `pump` finishes.
  - **`resume_gate_sample`.** Replace the `inflight.is_some()` early return with `inflight.len() >= resume_cap`. A passing sample (`passes(.., inflight.len() + 1)`) starts the next entry (`start_next`) and then calls `pump` to fill remaining slots from that same sample.
  - **`passes`** gains a parameter: `pub fn passes(p: &RecoveryParams, snap: Option<&HeadroomSnapshot>, sessions: u32) -> bool`, passing `sessions` to **both** `headroom::required_bytes(p.base_margin, p.per_session, sessions)` and `headroom::required_phys_bytes(p.per_session, sessions)`. Call sites: `evaluate_sample` (`1`), `resume_gate_sample` (`inflight.len() + 1`), A4 (`owed`), and the test-harness predicate in the `run()` helper (`Event::HeadroomTick(s) => passes(&m.p, s.as_ref(), …)`; use the same count the machine used). Update the `passes` tests (the `snap_mib` assertions) to pass `1`, and add `n = 2/3` cases.
  - **Failure paths with work in flight.**
    - A failed memory sample in `resume_gate_sample` parks every `Queued` entry as `Suspended(Gate)` (as today) and then: if `inflight.is_empty()` → `enter_await_headroom` (as today); otherwise **stay in `Resuming`** and return. The last in-flight `resume_result` → `pump` finds no `Queued` entry, calls `finish`, and `finish` already goes to `enter_await_headroom` while `Suspended(Gate)` entries remain. No new state is needed.
    - `pause()` requeues **every** in-flight key (table above).
    - `cancel_key` on one of several in-flight keys: marks only that entry `cancelled` (its result is later discarded, its key stays in `inflight` until the result arrives, and its `stats.total` is decremented as today).
    - **Crash accounting with several in flight (restored from the original proposal).** A herdr death must account for **every** in-flight key. The breaker counts deaths, not keys, so it needs no change. `halve_budget` already counts every `Resuming` queue entry, so all in-flight keys are charged against the halved budget. `process_death` must clear the whole `inflight` set and requeue or suspend every in-flight entry exactly as it does the single one today. Required test: a death with 3 in flight → the breaker is charged one death, `halve_budget` counts all 3, all 3 are requeued for the next episode, and their late `ResumeResult`s (old generation) are discarded.
- **Driver (`recovery/driver.rs`) — exact shape.**
  - New trait: `pub trait ResumeRunner: Send + Sync + 'static { fn resume(&self, key: &SessionKey, generation: Generation, mode: ResumeMode) -> Option<ResumeReport>; }`, with `pub struct ResumeReport { pub outcome: ResumeOutcomeKind, pub load_related: bool }` (see A3). `None` means "skipped: the generation moved on".
  - `Executor` loses `fn resume(&mut self, ..)` and gains `fn resume_runner(&self) -> Arc<dyn ResumeRunner>`. Implementors to change, all of them: `BackendExecutor` (backend mod), `FakeExecutor` (driver tests), `Idle` (recovery mod tests). `BackendExecutor::resume_runner` returns `Arc::new(BackendResumeRunner { parts: self.parts.clone() })`, where `BackendResumeRunner` holds a clone of `RecoveryParts` (it is `Clone` over `Send + Sync` ops) and implements `resume` with today's `BackendExecutor::resume` body, preceded by the generation re-check: `if parts.keymap.generation() != generation { return None }` — **before** `store.clear` and before any herdr call.
  - `Driver::new` gains two parameters: `tx: Sender<Event>` (where workers send results) and `dispatch: ResumeDispatch`, `pub enum ResumeDispatch { Threads, Inline }`. `Parked::spawn` passes `Threads` and a clone of the controller's sender (add a `tx: Sender<Event>` field to `Parked`, set in `controller()`). Test rigs (`rig` in driver tests, `Parked` tests in recovery mod) pass `Inline`.
  - `Command::Resume { key, generation, mode }` in `execute`:
    - `Inline`: call the runner synchronously and return `Some(Event::ResumeResult { key, generation, outcome, load_related })` as today (a `None` report returns `None`).
    - `Threads`: spawn a short-lived `std::thread` named `herdr-resume` that calls the runner and, when it returns `Some`, sends `Event::ResumeResult` on `tx` (ignore send errors); `execute` returns `None`. Worker panics are caught (`catch_unwind`) and reported as `Failed("resume panicked")` with `load_related = false`.
  - `Event::ResumeResult` gains `load_related: bool`; all constructors and matches in machine tests and `Sim` are updated.
  - **Driver lifetime (singletonized).** Because the driver now owns a `Sender`, its channel no longer disconnects when every `RecoveryHandle` is dropped. Add `Event::Shutdown` (machine ignores it in `handle`'s `_ => {}` arm), `RecoveryHandle::shutdown()` (sends it), have `Driver::run_with`/`run` stop when they receive it, and call `self.recovery.shutdown()` from `impl Drop for HerdrBackend` (backend mod).
  - The machine stays single-threaded and deterministic; only the side effects run in parallel.
- **Per-key locks.** The worker's `resume_session` takes the existing per-key spawn lock (`Spawner::lock_for`), so there is never more than one resume per key. Because of the generation check, a worker that outlives a herdr death skips its work instead of acting on a new server.
- **Queue order** is unchanged: mid-turn and recently busy sessions still start first.
- **Test harness.** The `run()` invariant helper in `machine.rs` tests: `assert!(in_flight <= 1, ..)` becomes `<= m.resume_cap as usize`, and `assert_eq!(in_flight == 1, m.inflight.is_some(), ..)` becomes `assert_eq!(in_flight, m.inflight.len(), ..)`. Tests that assume one in flight or a 5 s stagger are updated; at minimum:
  - `stagger_five_seconds_between_resumes` (rewritten for A3: no stagger after a clean resume)
  - `stale_generation_resume_result_ignored`, `start_fresh_for_inflight_is_ignored`, `cancel_inflight_marks_cancelled_and_ignores_result`, `focus_while_inflight_waits_for_inflight_then_runs_next` (compare `inflight` with `contains`/`is_empty`, not `Some(k)`/`None`), and `explicit_stop_clears_queue_and_stays_healthy_without_server`
  - the `Sim` helpers `auto_resume`, `pending`, `finish_pending`, `settle_until` (a pending `VecDeque` already holds several keys; `finish_pending` must answer each)
  - `driver.rs` `publish` and `gate_cell_published_before_commands_run`
  - Existing tests that need serial behaviour set `resume_concurrency = 1` in their `RecoveryParams` instead of being rewritten.

**A3. Stagger only on trouble; fall back to serial.**

- After a clean resume (`Resumed`, `Continued`, `StartedFresh`, `AlreadyRunning`), start the next one immediately: `resume_result` no longer sets `stagger_until`.
- **Decision (user: "go safer"):** the first **load-related** failure in an episode sets `resume_cap = 1` for the rest of the episode (until the reset sites listed in A2).
- **Load-related** is a boolean the runner computes (`ResumeReport.load_related`) and the machine receives in `Event::ResumeResult.load_related`. Exact rule, in `BackendResumeRunner::resume`:
  1. **Timeout:** the resume ended in `Failed`, or in an `Err`, whose text lowercased contains `"code":"timeout"` (herdr's JSON error code, seen in `keitora.log`) or `timed out waiting for agent startup`, or whose source is `HerdrError::Timeout` (herdr's own message is the observed one; both are matched). (singletonized)
  2. **Early-exit fallback:** the resume succeeded but the agent exited early and a weaker decision was used. This is exactly `fell_back` in `Spawner::spawn_agent_locked` (`decision != first`; it is only ever set by the `is_early_exit` arm). Expose it by changing `spawn_agent_locked` to return `Result<(u64, ResumeResult, bool), PtyBackendError>` (third value `fell_back`) and `resume_locked` to return `Result<Option<(ResumeResult, bool)>, PtyBackendError>`; adjust `Spawner::spawn_agent_with`, `Spawner::resume_session`, `HerdrBackend::spawn_agent` and `RecoveryParts::resume_session` accordingly (callers other than the runner ignore the flag).
  3. Everything else is **not** load-related, including a stored decision that was already `Fresh` because the transcript is missing (`fell_back` is false there), `NoSuchSession`, and `AlreadyRunning`.
- In `resume_result`, when `load_related` is true and `resume_cap > 1`, set `resume_cap = 1`. Non-load failures do not change the cap.
- **Stagger rule:** after every `Failed` outcome (load-related or not) **and** after any outcome with `load_related == true` (an early-exit fallback to continue or fresh, restored from the original proposal), `resume_result` sets `stagger_until = Some(now + p.stagger)`. After a clean outcome it does not. The stagger gates all new starts (it is one machine-wide deadline). `stagger_secs` keeps its meaning and default.
- **Honest estimate:**
  - No timeouts: the resume phase is about 24 × 4.3 s / 3 ≈ **35–45 s**.
  - With today's three timeouts at the head of the queue: the run goes serial early, at about 4.3 s each with no clean stagger, roughly **1.5–2 min**.
  - Either way, A1 makes most sessions usable well before the end. The serial fallback is a deliberate trade for safety, to be revisited with A5 data.

**A4. Skip the sustain when headroom is ample for the whole queue.**

- The first plan detected a reboot by comparing the remembered server's `start_time` with the system boot time. That was dropped:
  - Windows Fast Startup does not reset the boot time.
  - `start_time == 0` means "unreadable", which would look like a reboot.
- **Where the count comes from (singletonized).** The resume queue does not exist yet while waiting for headroom (it is built after reconcile), so the count of owed sessions is passed in with the boot event. `Event::BootRecovery` gains `owed: u32`; `RecoveryHandle::boot_recovery` gains the same last parameter. Callers in `RecoveryParts::boot` (backend mod, the four `handle.boot_recovery(..)` calls): `BootClass::RecoverDown` passes `self.keymap.should_be_running().len() as u32` (read right after `mark_owed()`); the other three call sites pass `0` (they either bypass the gate or reconcile without a sustain wait). Update every `BootRecovery` constructor in tests (`boot_live` and the other `Sim` helpers, `recording_handle` users).
- **Machine state.** `Episode` gains `boot_owed: u32` (set in `process_boot` only when `live.is_none() && !bypass_gate`, else `0`) and `boot_sustain_checked: bool` (default false). `Episode::default()` at `finish`/`explicit_stop` resets both.
- **Rule.** In `evaluate_sample`, before the existing `passes(.., 1)` logic: if `episode.boot && episode.boot_owed > 0 && !episode.boot_sustain_checked`, set `boot_sustain_checked = true`; if `passes(&p, s.as_ref(), episode.boot_owed)` (i.e. `required_bytes(base, per_session, N)` **and** `required_phys_bytes(per_session, N)`, with the existing "missing snapshot passes" and pressure rules) then call `proceed(now)` immediately (skipping the sustain). Otherwise fall through to the normal path with the normal sustain. Only the **first** sample of the episode's first wait is eligible; a later wait in the same episode (for example after `finish` re-enters `enter_await_headroom`) uses the normal sustain. A non-boot episode (`episode.boot == false`) never skips.
- This is per-episode state: nothing changes `self.p.sustain`.
- Today's evidence: 105 GiB of commit memory was available against about 18 GiB needed for 24 sessions, so it would have passed.
- The existing `bypass_gate` (used for `Fresh`/`DownNoSessions`) is not reused, because it skips the memory check entirely; A4 still requires a passing sample.

**A5. Timing.**

- **CLI call timing.** In `backend/herdr/real.rs`, wrap the `run_with_deadline(self.command(args), class.deadline())` call in both `RealHerdr::run` and `RealHerdr::run_raw` with an `Instant` timer and emit `log::debug!("herdr cli {} took {} ms", subcommand, ms)`, where `subcommand` is the first two entries of `args` joined by a space (for example `agent start`, `pane list`), or the first if there is only one. Log on success and on error. No value from the arguments beyond those two tokens is logged.
- **`LifecycleEvent::RecoveryTiming`.** Add to `src-tauri/src/lifecycle.rs`: `RecoveryTiming { sustain_ms: u64, resume_ms: u64, sessions: u32, failed: u32, final_cap: u32 }`, formatted by `format_event` as `event=recovery_timing sustain_ms=<n> resume_ms=<n> sessions=<n> failed=<n> final_cap=<n>` (plain integers, no quoting). Add a formatting test next to `recovery_phase_quotes_reason`.
- **Machine plumbing.** New `Command::RecordTiming { sustain_ms: u64, resume_ms: u64, sessions: u32, failed: u32, final_cap: u32 }`; the driver's `execute` maps it to `lifecycle::record(LifecycleEvent::RecoveryTiming { .. })` and returns `None`. The machine emits it from `finish`, on the path that reaches `Phase::Healthy` (not on the path that re-enters `enter_await_headroom`), only when the episode entered `Resuming` at least once. Values: `sustain_ms` = time from the first `enter_await_headroom` of the episode to the `proceed` call that ended that wait (0 when there was no wait); `resume_ms` = time from the first entry into `Resuming` to `finish`; `sessions` = `stats.total`; `failed` = `stats.failed`; `final_cap` = `resume_cap` at that moment. Track with two `Option<Instant>` fields on `Episode` (`await_first`, `resuming_first`) and one `Option<u64>` (`sustain_ms`).
- The first real reboot after merge also shows whether herdr serializes concurrent `workspace_create`/`agent_start` on its side, which would cap A2's gain.

### Out of scope (A)
- Replacing the fixed 300 ms `SHELL_READY_DELAY_MS`.
- Parallelising reconcile's `pane_close` loop.
- Root-causing the agent-startup timeouts.
- Any change to herdr itself.

---

## B. UBT activity hardening

### How it works today

`src-tauri/src/activity/progress/` reads only UBT's single shared log, `%LOCALAPPDATA%/UnrealBuildTool/Log.txt`:

- `tail.rs` keeps a byte offset and treats a new log as a new run **only if the file shrank**.
- `ubt.rs` parses attribution, step and result lines.
- `mod.rs` attributes a run by longest project-path prefix.
- A run ends **only** on a `Result:` line.
- A running bar stays visible while the log's file mtime is under 5 minutes old (`STALE_AFTER_MS`, `surface_decision`).
- Nothing looks at processes.

The frontend (`src/lib/sessions.ts`, polling every 2.5 s, then `Rail.tsx` `LaneBuild`) only renders what it is given, so **both bugs are in the backend**.

### Root causes
- **Stuck after cancel.** A killed UBT never writes `Result:`. The run stays `Running` (shown at up to 99%) and the lane keeps `working = true` until the shared log has been quiet for 5 minutes. Any later UBT write resets that clock.
- **Bar stays on the previous worktree.** When worktree B starts UBT, UBT rotates `Log.txt` to `Log-backup-*.txt` and starts a new file.
  - If the new file has already grown past A's offset by the next poll, no shrink is seen. That is likely, because a cancelled A run has a short log.
  - The tailer then seeks into the new file at A's old offset and skips B's `Log started at` and attribution lines.
  - B's `[n/total]` steps then land on A's still-open, attributed run.

### Design

**Conventions used below (singletonized).** All paths are under `src-tauri/src/activity/progress/` ("tail" = `tail.rs`, "ubt" = `ubt.rs`, "engine" = `mod.rs`, "procs" = the new `procs.rs`, to be declared with `pub mod procs;` in the engine). `ActivityState` in `src-tauri/src/activity/mod.rs` owns the `UbtEngine` and calls `UbtEngine::tick(&wt_paths, now_ms)`; that signature does not change. The frontend poll cadence (2.5 s) is the only tick source, so "probe" and "tick" counts below are in frontend polls.

**B1. Detect rotation by content identity.**

- **Primary signal.** `TailCursor` gains `head: Vec<u8>`, the fingerprint of the file head: the bytes from file offset 0 up to (not including) the first `\n`, capped at 256 bytes. (Normally the line `Log started at <timestamp>`.) If the file has no `\n` yet and fewer than 256 bytes, the fingerprint is whatever is there (a partial line).
  - It is captured on the first read (including the first-attach read, which may start mid-file: read the head separately from offset 0) and whenever the stored `head` is empty and the file now has bytes.
  - On every later read, compare the first `min(head.len(), current_len)` bytes of the file with `head`. Any mismatch is a rotation: set `offset = 0`, clear `partial`, replace `head` with the new head, set `reset = true`. Comparing only the common prefix avoids false resets while the file grows. If the stored `head` was a partial first line (no `\n` and < 256 bytes) and the file now holds more of the same first line, extend `head` after a matching compare.
  - Known, accepted limit: two runs whose first lines are byte-identical (the timestamp has one-second resolution) are not told apart by this signal; the secondary signal and the shrink rule are the backstops.
- **Secondary signal (weak).** `TailCursor` gains `created: Option<std::time::SystemTime>` from `Metadata::created()` (`None` when unsupported or erroring; then the signal never fires). If both the stored and the current value are `Some` and differ, that also resets. NTFS tunneling can carry the old creation time over to a re-created file, so this is never relied on alone. A reset caused by either signal is **one** reset (one `reset = true`), even when the shrink rule also applies.
- A shrink (`len < offset`) still resets, as today.
- `TailCursor::new()` initialises `head` empty and `created` `None`. `read_new_lines` keeps its signature and `TailRead { lines, reset }`; the existing tail tests are updated for the new fields, and the shrink and first-attach tests keep their assertions.

**B2. Process liveness (`progress/procs.rs`).**

- **Types and trait (exact).**
  - `pub struct UbtProc { pub pid: u32, pub project: Option<String> }`: a live **build-mode** UBT process; `project` is the normalized `-Project=` path (`None` = unknown: command line unreadable, or no `-Project=`).
  - `pub struct ProbeSnapshot { pub seq: u64, pub procs: Option<Vec<UbtProc>> }`. `procs = None` means the process list could not be read ("probe unavailable"). `seq` increases by one every time the probe really refreshed (not when it answered from its cache).
  - `pub trait UbtProbe: Send + Sync { fn snapshot(&self) -> ProbeSnapshot; }`.
  - `pub struct SysinfoProbe` is the real implementation (`sysinfo` 0.37, already a dependency; `ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet)`).
  - Tests use a fake (a `Mutex<ProbeSnapshot>` the test mutates). Existing engine tests that do not care use an "unavailable" fake (`procs: None`), which preserves today's behaviour.
- **Probe implementation.**
  - It refreshes process **names** first (`ProcessesToUpdate::All` with a `nothing()` refresh kind), then refreshes the command line only for candidate pids. Candidates (case-insensitive, with or without `.exe`): process name `UnrealBuildTool`; or process name `dotnet` whose arguments contain an entry ending in `UnrealBuildTool.dll`.
  - It reuses the scan pattern already in `backend/herdr/real.rs` (`server_pid` / `cmd_of`).
  - It caches its result for at least 2 s (a call within 2 s returns the previous snapshot with the same `seq`).
  - The engine calls it only when needed: when `run.attributed && run.finished.is_none() && !run.dead`, **or** when the log file's mtime is within the last 30 s. Otherwise it is not called and the miss streak (below) is reset.
- **Argument parsing (in `ubt.rs`, next to `extract_project_from_cmdline`).**
  - `pub fn extract_project_from_args(args: &[String]) -> Option<String>`: finds the first argument starting with `-Project=` (case-insensitive), strips one surrounding pair of `"` or `'` from the value, trims, and returns `normalize_either_separator(value)`; `None` if absent or empty. Because it works on one already-split argument, paths with spaces survive. The result goes through `project_to_worktree` as for log lines.
  - `pub fn is_build_mode(args: &[String]) -> bool`: **rule: no `-Mode=` argument, or `-Mode=Build` (case-insensitive), counts as build; any other `-Mode=` value (for example `QueryTargets`, `JsonExport`, `GenerateProjectFiles`, which IDEs run in the background) is ignored.** No fixture or source in the repo names other mode values, so this is stated as a rule rather than a list. (singletonized)
  - A candidate process with `is_build_mode` true and no readable command line (empty `cmd()`) is classed as build-mode with `project: None` (**unknown**).
- **Engine state (singletonized).** `Run` gains `dead: bool` and `last_line_ms: i64`; `Inner` gains `pending: Option<PendingAction>`, `streak: u8` and `last_probe_seq: u64`, with `enum PendingAction { Cancel, Reattribute(String) }` (the action the streak is counting toward and its target worktree).
- **Rules**, applied once per tick **after** draining the tail and **after** the `read.reset`/header handling, and only on a **new** snapshot (`snapshot.seq != last_probe_seq`; a repeated `seq` changes nothing and does not advance the streak). A run whose `finished` is `Some` is skipped entirely, so a late `Result:` always wins. Evaluate in this order, with `W` = the run's worktree and `mapped` = the set of distinct worktrees named by live build processes whose `project` maps via `project_to_worktree`:
  1. **Probe unavailable** (`procs == None`): no rule applies; reset the streak; the run is treated as process-confirmed for B4 (falls back to today's 5-minute allowance). (singletonized)
  2. **Confirmed:** `W ∈ mapped` → reset the streak; the run is process-confirmed for B4.
  3. **Unknown:** any live build process with `project == None` → reset the streak; the run is kept alive but not process-confirmed, and is never reattributed or cancelled.
  4. **Reattributed:** (rules 1–3 did not match) and `mapped` holds **exactly one** worktree `X` (with `W ∉ mapped`, or the run unattributed) → candidate `Reattribute(X)`.
  5. **Cancelled:** (rules 1–4 did not match) the run is **attributed** → candidate `Cancel`. That is: no live build UBT maps to `W`, no unknown build UBT is live, and `mapped` is empty or holds two or more other worktrees (ambiguous: nothing is reattributed, the stale run is still dropped). An **unattributed** run with no candidate just waits (no action).
  - **Streak.** A candidate counts only when the same `PendingAction` (same target worktree) was the candidate on the previous **new** snapshot: `streak` goes 1 → 2. At `streak == 2`, apply it. Any tick that yields no candidate (rules 1–3, or no new snapshot is **not** such a tick: a repeated `seq` leaves `pending`/`streak` untouched) resets `pending = None, streak = 0`. (This is the plan's "2 consecutive probes", and it also applies to reattribution. singletonized)
  - **Cancel applies:** the run is **dropped**: replace it with `Run { dead: true, ..Run::default() }` (no worktree, not attributed). Nothing is surfaced: the bar clears immediately, as the user chose, and `working` drops. While `dead`, `apply` ignores every delta (steps, totals, results, attribution lines). Only a `Log started at` line, a rotation reset (B1), or B2's reattribute action creates a new run (`start_new_run` builds `Run::default()`, which is not dead).
  - **Reattribute applies:** drop the old run as above and start a new run (`start_new_run(now_ms)`) with `worktree = Some(X)`, `attributed = true`; it shows no progress until its next step lines arrive. An **unattributed** running run (from B3, or a header with no attribution line) is attributed the same way, but only when `mapped == {X}` **and** no unknown build UBT is live. (singletonized)
  - **Dead-run edge (singletonized):** `Run.last_line_ms` is set to `now_ms` whenever a line is read while the run is not dead, and when a run starts.
- **Wiring.**
  - `UbtEngine::new(log_path: Option<PathBuf>, probe: Arc<dyn UbtProbe>)`. `impl Default for UbtEngine` (used by `ActivityState`) passes `Arc::new(SysinfoProbe::default())`. Update the six `UbtEngine::new(Some(..))` calls in the engine's tests to pass the unavailable fake.
  - The module doc at the top of the engine is updated: the single-instance mutex means "at most one live build", but the log is not the only signal any more.

**B3. Run-consistency guard.** In `Inner::apply`, for an **attributed, unfinished** run with `run.total != 0`: a `ProgressDelta::Total(n)` with `n != run.total`, a `ProgressDelta::Step { total: Some(t), .. }` with `t != run.total`, or a `ProgressDelta::Step { done, .. }` with `done < run.done` means the steps belong to another build. The current run is closed (dropped, **not** marked `dead`, not surfaced as finished) and `start_new_run(now_ms)` starts a new **unattributed** run (`attributed = false`, `worktree = None`). The offending delta itself is discarded (an unattributed run ignores steps, as `apply` already does). It gets no credit until a log attribution line or B2 (reattribute rule above) names its worktree, and its progress then starts from the next step lines. A first `Total` or `Step` on a run whose `total == 0` is never a violation. (singletonized: the discarded-delta behaviour.)

**B4. Run-scoped staleness.**

- `surface_decision` changes from `(finished, has_progress, age_ms)` to `fn surface_decision(finished: bool, has_progress: bool, run_age_ms: i64, process_confirmed: bool) -> bool`.
  - `run_age_ms` is computed in `tick` as `max(now_ms - run.last_line_ms, now_ms - log_mtime_ms)` (a missing mtime counts as `i64::MAX`). Taking the maximum keeps the old protection against replaying a long-finished log at startup (old mtime) and adds run scoping (another writer keeping the file's mtime fresh cannot keep this run alive). (singletonized)
  - Finished run: shown while `run_age_ms <= FINISHED_RETENTION_MS` (20 s, unchanged).
  - Running run: shown only with `has_progress` and `run_age_ms <= STALE_AFTER_MS` (5 min, unchanged) when `process_confirmed`, else `run_age_ms <= UNCONFIRMED_STALE_AFTER_MS`, a new const of **90 000** ms. Long link steps are quiet, hence the 5-minute allowance while B2 confirms a matching live process (rule 2, or the probe-unavailable fallback). An *unknown* live UBT (rule 3) keeps the run from being cancelled but does **not** extend the window beyond 90 s. (singletonized)
  - `Inner::surface` takes `(now_ms, run_age_ms, process_confirmed)` instead of `age_ms`; `Inner` records `process_confirmed` from the latest rule evaluation (default false; true when the probe is unavailable).
- The existing unit tests of `surface_decision` are updated to the four-argument form, plus cases for the confirmed/unconfirmed windows.

### Out of scope (B)
- Per-worktree `-Log=`; Keitora only observes builds.
- Source-engine log locations.
- Tailing `Log-backup-*.txt`.
- Frontend changes. `LaneBuild` already hides when `build` is absent.

---

## C. Terminate / Launch sessions

### How it works today
- **Kill.** `pty_kill` (`src-tauri/src/host.rs`) → `HerdrBackend::kill` cancels recovery for the key, resolves it out of `should_be_running` and drops it from the keymap. Recovery mostly won't resume it.
- **The frontend respawns it.** Every worktree pane of a warm project mounts, and `doAttach` falls back to `spawn()` on `NoLiveSession` (`src/components/TerminalPane.tsx`). So a killed session comes back on the next app launch. **This auto-spawn is the main thing the flag must gate.**
- **Resume store.** `ResumeStore` (`backend/herdr/resume.rs`, over `keitora_tasks::LocalStore`, `<KEITORA_HOME>/herdr/resume.db`) keeps the last conversation id across a kill. A plain non-fresh spawn resumes it through the existing Resume → Continue → Fresh fallbacks.
- **Context menu.** The only one is `WorktreeMenu` on rail rows (`src/components/Rail.tsx`). It holds Restart / Start fresh / Transfer, and restarts are signalled to the pane through nonces (`App.tsx` `bumpRestartNonce`).
- **`WorktreeInfo`** exists only in TypeScript (`src/lib/ipc.ts`). The Rust side is `keitora_tasks::ResolvedWorktree`, returned by the stateless `worktrees()` command in `host.rs`.

### Design

**Conventions used below (singletonized).** Rust paths are under `src-tauri/src/` unless they start with `crates/`. "Resume store" is `ResumeStore` in `backend/herdr/resume.rs`; "backend mod" is `backend/herdr/mod.rs`. A key's identity is `SessionKey { project, worktree }`; `terminated` always means "the persisted flag is 1 for `(project, worktree)`".

**C1. Persisted flag: a separate table.**

- Add `session_flags(project TEXT NOT NULL, worktree TEXT NOT NULL, terminated INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL DEFAULT (datetime('now')), PRIMARY KEY(project, worktree))`, created with `CREATE TABLE IF NOT EXISTS` inside `init_schema` in `crates/keitora-tasks/src/sqlite_backend.rs`, next to `resume_sessions`. Like `resume_sessions`, only the global resume DB uses it; project DBs carry it empty.
- It is a separate table, not a column on `resume_sessions`, because:
  - That table's `cli NOT NULL CHECK(...)` and `observed_at NOT NULL` columns make "create a flag row for a key with no resume record" awkward.
  - A separate table needs no `ALTER` migration and has no race between two processes.
- **`LocalStore`** (`crates/keitora-tasks/src/local_store.rs`) gains, all synchronous and returning the crate's `Result`:
  - `pub fn set_terminated(&self, project: &str, worktree: &str, terminated: bool) -> Result<()>`: an upsert (`INSERT … ON CONFLICT(project, worktree) DO UPDATE SET terminated = excluded.terminated, updated_at = datetime('now')`). Clearing a flag on a key with no row is a harmless insert of `0`.
  - `pub fn is_terminated(&self, project: &str, worktree: &str) -> Result<bool>` (no row = `false`).
  - `pub fn terminated_in(&self, project: &str) -> Result<Vec<String>>`: worktree names with `terminated = 1`, ordered by name.
- **`ResumeStore`** wraps them over `with_db_do`:
  - `pub fn set_terminated(&self, key: &SessionKey, terminated: bool) -> Result<(), String>`: `Err` when there is no DB or the write failed (so a terminate never silently fails to persist). (singletonized)
  - `pub fn is_terminated(&self, key: &SessionKey) -> bool`: `false` when there is no DB or the read failed.
  - `pub fn terminated_in(&self, project: &str) -> HashSet<String>`: empty on failure.
  - `pub fn skip_resume(&self, key: &SessionKey) -> bool` = `agent_quit(key) || is_terminated(key)`.

**C2. Recovery exclusion.**

- Replace the direct `agent_quit` reads with `skip_resume` at exactly these four sites (nothing else reads `agent_quit` outside `resume.rs` and tests):
  1. `note_server_death` (backend mod): the `.filter(|k| … && !self.store.agent_quit(k))` over the live set.
  2. `RecoveryParts::mark_owed` (backend mod): the `.filter(|k| !self.store.agent_quit(k))`.
  3. `reconcile_lineage` in `backend/herdr/reconcile.rs`, the adopt branch (`if running || ctx.store.agent_quit(&k) || !(ctx.worktree_exists)(&k)`).
  4. `reconcile_lineage` in `reconcile.rs`, the `quit` set built for `keys_to_resume` (`.filter(|k| ctx.store.agent_quit(k))`).
- Add a backstop at the top of `Spawner::resume_locked` (backend mod), before the "already runs again" check: if `self.store.is_terminated(key)`, call `self.keymap.resolve_recovered(key)` and return `Ok(None)` (the runner then reports `AlreadyRunning`, which counts as neither resumed nor failed). This covers:
  - "Resume all" (`recovery_resume_now`)
  - resume-on-focus (`Event::UserFocus` / `UserStartFresh`)
  - a terminate that races an in-flight resume, since `cancel_key` only marks the entry
  - a terminated key that still got into the queue through any other path

**C3. IPC and backend.**

- **`PtyBackend`** (`backend/mod.rs`) gains two required methods (no default bodies):
  - `fn terminated_keys(&self, project: &str) -> HashSet<String>;` (worktree names)
  - `fn set_terminated(&self, key: &SessionKey, terminated: bool) -> Result<(), PtyBackendError>;`
  - Implementors, exhaustively (`grep 'impl PtyBackend for'`): `HerdrBackend` (`backend/herdr/mod.rs`; delegates to `self.store`, mapping the store's `Err(String)` to `PtyBackendError::Other`), `UnavailableBackend` (`backend/unavailable.rs`; `terminated_keys` returns an empty set, `set_terminated` returns `Err(PtyBackendError::Other("herdr unavailable"))`), and `FakeBackend` (`backend/fake.rs`, `#[cfg(test)]`; an in-memory `Mutex<HashSet<SessionKey>>`). There are no other implementations.
- **`PtyBackendError::Terminated`** (new variant, `backend/mod.rs`): `Display` is `"Terminated: the session was terminated"` (it contains `Terminated`; the frontend matches the wire token below, not this text). Update the exhaustive `Display` impl; `map_write_line_result` already treats unknown errors as `Error`.
- **`session_terminate(project, id, gen)`** — a new `#[tauri::command(async)]` in `host.rs`, registered in `generate_handler!` next to `pty_kill`. Signature: `fn session_terminate(client: State<SharedBackend>, app: tauri::AppHandle, project: String, id: String, gen: Option<u64>) -> Result<(), String>`.
  1. Set the flag **first**: `client.set_terminated(&key, true)`; on `Err` return the error string and do nothing else.
  2. Resolve the generation: `gen` if given, else the `gen` of the key's entry in `client.cached_sessions()`. If the key has no live entry, skip the kill. If `cached_sessions()` itself is refused with `RecoveryPending`/`RecoveryPaused`, skip the kill too.
  3. Run the existing kill path: `client.kill(&key, gen)`. If the kill is refused with `RecoveryPending`/`RecoveryPaused` (terminate during recovery), the flag is still set and the command **succeeds**; the `resume_locked` backstop guarantees the key is not resumed, and the UI shows it as terminated. Any other kill error is returned (the flag stays set).
  4. Emit `worktrees://changed` and `sessions://changed` (`app.emit`, as `host.rs` does elsewhere).
- **`session_launch(project, id, cwd, rows, cols, fresh)`** — a new `#[tauri::command(async)]` in `host.rs`, registered in `generate_handler!`. Signature: `fn session_launch(client: State<SharedBackend>, app: tauri::AppHandle, project: String, id: String, cwd: String, rows: u16, cols: u16, fresh: Option<bool>) -> Result<u64, String>`.
  1. `client.set_terminated(&key, false)`; on `Err` return it and do not spawn.
  2. Emit `worktrees://changed`.
  3. Spawn through the normal path: `spawn_pty(client.inner().as_ref(), project, id, cwd, rows, cols, None, fresh.unwrap_or(false))`; return its generation. With `fresh = false` it resumes the last conversation through the existing Resume → Continue → Fresh fallbacks.
  - If the spawn then fails (for example `Recovering`), the flag stays cleared. This is accepted: the pane shows the normal spawn-failed or recovering state.
  - The spawn takes the per-key lock (`Spawner::lock_for`), so a launch right after a terminate waits for the old kill and bridge teardown.
- **Refusal backstop.**
  - `HerdrBackend::spawn` and `HerdrBackend::spawn_agent` refuse a terminated key: right after their `self.gate.admit(None)?` they return `Err(PtyBackendError::Terminated)` when `self.store.is_terminated(&key)`. (`resume_locked` calls `spawn_agent_locked` directly, so it is covered by its own backstop, not this one. `HerdrBackend::attach` does **not** check the flag: an attach for a terminated key already ends in `NoSuchSession`, and the pane's `terminated` prop prevents the follow-up spawn. singletonized)
  - It is mapped in `host.rs`: add `const TERMINATED_TOKEN: &str = "Terminated";` next to `RECOVERING_TOKEN`; `spawn_err_kind` and `attach_err_kind` both map `PtyBackendError::Terminated` to it. Update their tests (`attach_err_kind_tokens`, `spawn_err_kind_tokens`).
  - `src/lib/attachErrors.ts`: `AttachErrorAction` gains `"terminated"`; `classifyAttachError` returns it when `msg.includes("Terminated")`, checked **after** the `Recovering` check and **before** the `NotConnected`/`spawn` default. Update the doc comment's token list and `attachErrors.test.ts`.
- **`worktrees()` command** (`host.rs`). It becomes `fn worktrees(client: State<SharedBackend>) -> Result<Vec<WorktreeView>, String>`, where `WorktreeView` is a new Tauri-layer struct `#[derive(Serialize)] struct WorktreeView { #[serde(flatten)] inner: keitora_tasks::ResolvedWorktree, terminated: bool }`. It takes the active project's name from the same `resolve_active()` result it already uses (`pc.project.name`, cloned before `Config::from_project(pc)`), calls `client.terminated_keys(&project)`, and sets `terminated` per worktree name. With no active project it still returns an empty list. The tasks crate's `ResolvedWorktree` is not changed. `WorktreeInfo` in `src/lib/ipc.ts` gains `terminated: boolean`; every `WorktreeInfo` literal in TypeScript tests/fixtures gets `terminated: false`.
- **Transfer.** In `transfer_worktree` (`src-tauri/src/transfer.rs`), right after the `key` is built and **before** `with_recovery_lock` (so before any registry mutation), return `Err("session is terminated; launch it before transferring".into())` when `client.terminated_keys(&key.project).contains(&key.worktree)`.
- **Dispatch (`src-tauri/src/dispatch.rs`, `dispatch_apply`).** A terminated worktree normally has an `offline` session row, so `decide(..)` returns `Decision::Queued` and `write_line` is never reached; it can also reach `write_line` and get `NoSuchSession`. Handle both: in the `Decision::Queued` branch, when the worktree is terminated (`client.terminated_keys(&project).contains(&worktree)`), the reason text is `"{worktree} is terminated — reserved"` instead of `"{worktree} busy — reserved"`; in the `Dispatched` branch, when `result` is `Err(PtyBackendError::NoSuchSession)` and the worktree is terminated, `detail` is `"session is terminated; launch it to dispatch"`. Nothing else about dispatch changes (it still never errors after reserving). (singletonized)

**C4. UI.**

- **`WorktreeMenu`** (`src/components/Rail.tsx`):
  - `WorktreeMenuState` becomes `{ name; x; y; cli; terminated: boolean }`, filled where the menu is opened from the row's `WorktreeInfo.terminated`.
  - A live session gets **Terminate session**, with a confirmation like Restart's (a third confirm flavour in the pane, below). It is listed after Transfer.
  - A terminated session gets **Launch** and **Launch fresh**, and hides Restart session, Start fresh conversation and Transfer.
  - New props on `WorktreeMenu` and `Rail`: `onTerminateSession: (worktree: string) => void`, `onLaunchSession: (worktree: string, opts?: { fresh?: boolean }) => void`.
- **Rail lane:** the `terminated` flag selects a dimmed `terminated` style (a new `.wt-block.terminated` rule in `src/App.css`, distinct from `offline`; use `--offline` at reduced opacity) and a "Terminated" tooltip. It wins over every other lane status (`waiting`, `working`, `session.state`, `offline`) in the lane status helper in `Rail.tsx` (the function that ends in `return available ? "idle" : "offline"`), since a terminated session has no agent.
- **`src/lib/paneStatus.ts`:** `Status` gains `"terminated"` (comment: "the user terminated the session; stays down until launched"); it is added to `ALL_STATUSES` and is in none of `LIVE`/`RECOVERABLE`/`WAITING`. `nextPaneStatus` starts with `if (status === "terminated") return { status, attach: false };` (before the `suspended` check), so it never re-attaches from it and recovery never overrides it; `afterRecoveringRefusal` inherits this. Update `paneStatus.test.ts` (it enumerates `ALL_STATUSES`).
- **`TerminalPane` (`src/components/TerminalPane.tsx`):** new props `terminated: boolean`, `terminateNonce: number`, `launchNonce: number`, `launchFresh?: boolean`.
  - `terminated` is the single source of truth. A `terminatedRef` mirrors it; `doAttach` and `spawn` return immediately when it is set (so no auto-spawn on `NoLiveSession`, and a restart nonce is ignored: the restart-nonce effect returns early while `terminatedRef.current`). The pane starts in `"terminated"` status when the prop is initially true.
  - An effect on the prop: becoming `true` → `genRef.current = null`, `updateStatus("terminated")`; becoming `false` while the status is `"terminated"` → `doAttach()` (a launch from elsewhere).
  - A terminated pane shows a "Session terminated" overlay with **Launch** / **Launch fresh** buttons (modelled on the existing ended/spawn-failed overlay and its `restartBtnRef` focus handling).
  - A `Terminated` refusal token, from a stray spawn (in `spawn`'s `catch` and in `doAttach`'s `classifyAttachError` handling, action `"terminated"`), also lands in `"terminated"` status.
  - `terminate()` is modelled on `restart()` but never respawns: call `sessionTerminate(projectName, name, genRef.current ?? undefined)` (best-effort, as `restart` does for `ptyKill`), set `genRef.current = null`, reset the coalescer, set status `"terminated"` (the terminal's scrollback stays visible behind the overlay).
  - `launch(fresh)` is `spawn(fresh)` with one difference: it calls `sessionLaunch(projectName, name, path, term.rows, term.cols, fresh)` instead of `ptySpawn`. Implement by giving `spawn` a second parameter `via: "spawn" | "launch" = "spawn"`.
  - Nonce effects mirror the restart-nonce effect (seeded `lastSeen…Ref`, act only when the nonce increases): `terminateNonce` → if the status needs confirmation (`needsConfirm`), open the confirm with a new `confirmKill` flavour `"terminate"` (state type becomes `false | "resume" | "fresh" | "terminate"`, with copy from a new `terminateConfirmCopy()` next to `restartConfirmCopy` in `src/lib/resumeNotice.ts`), else `terminate()`; `launchNonce` → `launch(launchFresh)` (no confirm).
- **`App.tsx`:** `onTerminate` / `onLaunch` callbacks are wired into `<Rail>` (`onTerminateSession`, `onLaunchSession`) and `<TerminalPane>` (`terminateNonce`, `launchNonce`, `launchFresh`, `terminated` = the matching `WorktreeInfo.terminated`) with the existing nonce pattern: two new `Map<string, RestartSignal>` states built with `nextRestartSignal` (as `restartNonces` is; `fresh` is ignored for terminate), each handler first focuses the target pane exactly like `onRestartSession` does. The worktree list refreshes through `worktrees://changed` (`useLiveResource`), so `terminated` updates after the command's emit.

### Out of scope (C)
- Bulk terminate/launch.
- Auto-terminate policies.

---

## Tests (`#[cfg(test)]` / vitest, same change as the logic)

**Recovery machine and driver** (`Sim`, `FakeExecutor`/`Rig`; no real herdr):
- the concurrency cap is respected, and queue order is preserved
- `passes(n)` scales both commit and physical requirements
- no stagger after a clean resume
- the first timeout or early exit drops the cap to 1 for the episode, and a non-load failure does not
- a memory-sample failure with work in flight drains before going to `AwaitHeadroom`
- `ResumeResult` with a stale generation is ignored
- `pause` requeues all in-flight keys
- `cancel_key` on one of several in-flight keys
- `finish` waits for an empty in-flight set
- `settled` contains only successful keys
- a death with 3 in flight: one breaker death, `halve_budget` counts all 3, all 3 requeued, late old-generation results discarded
- an early-exit fallback (`load_related`) sets the stagger and drops the cap to 1
- the gate admits settled and in-flight keys during `Resuming`, and refuses queued, failed and keyless calls
- A4: a boot sample that passes for N sessions skips the sustain; one that passes for 1 but not N keeps it; a non-boot episode always keeps it
- `RecoveryTiming` formatting (`event=recovery_timing sustain_ms=… resume_ms=… sessions=… failed=… final_cap=…`), and the machine emits `Command::RecordTiming` once on the `Resuming → Healthy` finish only
- `resume_concurrency` default, serde round-trip, and the clamp to ≥ 1
- `load_related` classification: a timeout failure and an early-exit fallback are load-related; a stored `Fresh` (missing transcript) and `AlreadyRunning` are not
- `settled` gains `AlreadyRunning` but not `Failed`, is cleared by death/boot/user-restart/explicit-stop and not by `finish`, and loses a cancelled key
- stagger follows every `Failed` outcome and no other outcome
- driver: `ResumeDispatch::Threads` sends `Event::ResumeResult` from a worker; a worker whose generation moved on sends nothing; `Event::Shutdown` stops the driver
- gate: `admit_session` is refused while `Paused`, admits settled keys only in `Resuming`, and the default trait body behaves as `admit(Some(key))`
- `BootRecovery { owed }`: only `RecoverDown` passes a non-zero count

**Tailer:**
- rotation to a **longer** file resets via the fingerprint
- a fingerprint compared on a growing file does not false-reset
- a creation-time change resets, and an unsupported/`None` creation time never does
- a partial first line that later completes does not false-reset
- an empty file then a first write captures the fingerprint without a reset
- existing shrink and first-attach behaviour is unchanged

**Progress engine** (fake `UbtProbe`):
- a cancelled run (no `Result:`, process gone for 2 probes) → bar cleared, `working` false
- a late `Result:` after the process exits → the result wins
- A cancelled, then B starts with a rotated longer log → bar on B only
- B's steps with a missed header are not credited to A
- a `total` change mid-run → new unattributed run
- an unknown live UBT keeps the run and never reattributes it
- a query-mode UBT (`-Mode=QueryTargets`) is ignored; `-Mode=Build` and no `-Mode=` count as build
- a dropped run is not resurrected by later lines
- the argument-aware `-Project=` parse handles paths with spaces and a quoted value
- the streak needs 2 **new** snapshots (same `seq` twice does not count); an unavailable probe never cancels and keeps the 5-minute window
- exactly one other worktree live for 2 snapshots reattributes; two other worktrees live drops the run without reattributing; an unknown UBT blocks both
- an unattributed run is attributed by B2 only when exactly one build UBT is live and it maps
- `surface_decision` uses the run age and process confirmation (90 s vs 5 min), and a long-finished log at startup still shows nothing

**Terminate / Launch:**
- `session_flags` set, clear and query, including a key with no resume row
- `skip_resume` at all four sites
- the `resume_locked` backstop
- `spawn` / `spawn_agent` refuse a terminated key
- launch clears the flag and spawns
- terminate during recovery still sets the flag
- the transfer refusal leaves the registry unchanged
- `worktrees()` carries `terminated`
- `session_terminate` with `gen = None` resolves the generation from `cached_sessions`, skips the kill for a key that is not live, and still succeeds when the kill is refused with `Recovering`
- `ResumeStore::set_terminated` returns `Err` when there is no DB
- `dispatch_apply` reports "terminated" in both the `Queued` and the `NoSuchSession` paths
- frontend: `nextPaneStatus` keeps `terminated` sticky, `classifyAttachError` returns `terminated`, `paneRecoveryState` handles `settled`, and the `recovery.test.ts` cases are kept

**Fixtures:**
- `ubt_cancelled.log` (no `Result:`)
- a two-run rotation pair
- generic names only (`proj`, `MyGame`, `alpha`/`beta` worktrees). This repo is open source.

**Verification** (each run once): `cargo test --workspace`, `yarn test`, `yarn build` (tsc), and biome. The app smoke test runs only via `yarn dev:isolated` with the user's go-ahead.

## Dependencies
- `sysinfo` 0.37, already present.
- No herdr or protocol changes.
- One new config field (`resume_concurrency`) and one new SQLite table (`session_flags`).
- Tauri-layer additions only (no herdr protocol change): `daemon://recovery` gains `settled`; commands `session_terminate` / `session_launch`; `worktrees` gains `terminated`.

## Risks
- **Load-induced timeouts or silent fallbacks.** Concurrent agent starts can time out, or fall back to continue or fresh, which loses the resumed conversation. Mitigated by the cap of 3, the serial fallback after the first load failure (A3), and A5 data.
- **herdr-side serialization.** If herdr serializes concurrent starts internally, A2's gain shrinks. A1 and A4 still deliver most of the perceived win. A5 will show it.
- **State-machine regressions.** Going from a single in-flight key to a set touches every in-flight transition. Mitigated by the audit list in A2 and the expanded `Sim` tests.
- **Process probe cost.** It is bounded by the name-first refresh, the 2 s cache and the activity gating in B2.
- **UBT invocation shapes.** IDE and script-launched UBT may lack `-Project=`. The unknown rule prevents false cancels; the build-mode filter prevents false reattribution.
- **The open-source repo.** `plans/` was deleted from the public repo during the cutover. This plan is a working document on the branch only. Whether Finalization archives it in-repo is a review-gate decision. New fixtures and comments use generic names only.

## Decisions (resolved in Refine)
1. `resume_concurrency = 3`.
2. After the first load-related failure, the rest of the episode resumes serially, which is the safer option (A3).
3. Progressive attach is confirmed: only successfully resumed sessions unlock early (A1).
4. Terminate / Launch is in scope (C).
5. *(review)* Reboot detection was replaced by the "ample headroom for the whole queue" check (A4).
6. *(review)* The terminated flag lives in a new `session_flags` table, not on `resume_sessions` (C1).

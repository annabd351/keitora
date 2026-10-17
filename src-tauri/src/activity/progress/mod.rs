//! Build-progress engine for the per-worktree activity surface.
//!
//! Installed-engine Unreal Build Tool writes a single shared log
//! (`%LOCALAPPDATA%/UnrealBuildTool/Log.txt`) and holds a single-instance mutex, so at most
//! one build runs at a time. The engine tails that one file, attributes the current run to a
//! worktree by its logged project path, and publishes `done/total`, phase, `%`, ETA and
//! state as a [`BuildProgress`] — reading the build's own output, never wrapping it.
//!
//! State lives in one [`UbtEngine`] (a `TailCursor` + the current run), not per-worktree:
//! the shared log only ever describes one run. A `Log started at` line or a log rotation
//! starts a fresh run.
//!
//! The log is not the only signal. A killed build writes no `Result:`, and a rotation can
//! hide the next build's header, so a [`procs::UbtProbe`] reports which build-mode UBT
//! processes are alive and for which project:
//!
//! - an attributed run with no live UBT for its worktree (and no unknown UBT) is dropped
//!   after two consecutive new probe snapshots — no "cancelled" bar, it is simply gone;
//! - a run whose worktree is not building while exactly one other worktree is, is
//!   reattributed to that worktree (also after two snapshots);
//! - a build-mode UBT whose project cannot be mapped to a registered worktree (no
//!   `-Project=`, a relative or short-form path, a foreign root) counts as unknown: it keeps
//!   the current run alive and blocks both cancellation and reattribution;
//! - a changed action total or a step count going backwards stays in the same run (one UBT
//!   invocation can print several action batches); the counters follow the new batch.
//!   Steps from a build whose header was missed are corrected by the log-rotation reset and
//!   the probe's reattribution, not by splitting runs.
//!
//! Staleness is scoped to the run (its last line) as well as the file's mtime, so a
//! long-finished run already in the file does not paint a bar on startup, and another
//! writer keeping the file fresh cannot keep a dead run alive. A running bar survives five
//! quiet minutes only while a matching process confirms it (or the probe is unavailable);
//! otherwise 90 s.

pub mod procs;
pub mod tail;
pub mod ubt;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Lifecycle of a tracked build.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BuildState {
    Running,
    Succeeded,
    Failed,
}

/// Live progress of a long-running build in a worktree, surfaced on the rail.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct BuildProgress {
    /// Adapter id, e.g. `"ubt"`.
    pub adapter: String,
    /// Display label, e.g. `"UBT · Compile"` (adapter id uppercased + phase) or `"UBT"`.
    pub label: String,
    pub done: u32,
    /// `0` until known.
    pub total: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub state: BuildState,
    /// Epoch ms when the first progress line arrived.
    pub started_at: i64,
    /// Seconds remaining; `None` while hidden (first few actions) or unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u32>,
    /// `0..=100`, already capped (99 while running; 100 on success).
    pub percent: u8,
    /// Failure summary (first error line), when failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One parsed progress event from the UBT parser.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressDelta {
    Total(u32),
    Step {
        done: u32,
        total: Option<u32>,
        phase: Option<String>,
    },
    Succeeded,
    Failed(Option<String>),
    /// A resolved project path (shared-log attribution).
    Attribute(String),
}

/// EWMA smoothing factor for the action rate.
const ALPHA: f64 = 0.2;
/// Minimum seconds between rate samples.
const MIN_SAMPLE_DT: f64 = 0.5;
/// A stall longer than this feeds a `0` rate sample (ETA lengthens on long links).
const STALL_GRACE: Duration = Duration::from_secs(5);
/// ETA ceiling; above this, ETA is hidden.
const ETA_CAP_SECS: u64 = 86_400;
/// Hide ETA until at least this many actions are done.
const ETA_MIN_DONE: u32 = 5;

/// Keep showing a finished (`Result:`) run while the log's last write is within this window,
/// so a Succeeded/Failed bar lingers briefly — but a long-finished run already in the file on
/// startup (old mtime) is not shown at all.
const FINISHED_RETENTION_MS: i64 = 20_000;
/// Treat a running build whose log has not grown within this window as stale (crashed/killed
/// builds write no `Result:`). Generous, because a single long link step emits no interim
/// lines.
const STALE_AFTER_MS: i64 = 300_000;
/// The quiet window for a running build that no live process confirms (an unknown UBT is
/// alive, or nothing is and the run has not been dropped yet).
const UNCONFIRMED_STALE_AFTER_MS: i64 = 90_000;
/// Ask the process probe while the log was written this recently, even with no live
/// attributed run (a new build may be starting, or its header was missed).
const PROBE_RECENT_LOG_MS: i64 = 30_000;
/// Consecutive new probe snapshots that must agree before a run is dropped or reattributed.
const PROBE_STREAK: u8 = 2;

/// The current (single) build run accumulated from the shared log.
#[derive(Default)]
struct Run {
    total: u32,
    done: u32,
    phase: Option<String>,
    started_at: i64,
    finished: Option<BuildState>,
    message: Option<String>,
    /// Set once the run's project is confirmed to map to a known worktree.
    attributed: bool,
    /// The worktree this run belongs to (the mapped registry name).
    worktree: Option<String>,
    /// Dropped (its build is gone): every later delta is ignored until a new run starts.
    dead: bool,
    /// Epoch ms the run started or last received a line (run-scoped staleness).
    last_line_ms: i64,
}

/// What the probe streak is counting toward.
#[derive(Debug, Clone, PartialEq)]
enum PendingAction {
    /// Drop the run: its build is gone.
    Cancel,
    /// Move the run to the one worktree that is actually building.
    Reattribute(String),
}

/// Managed engine state: the single tail cursor + current run, behind one mutex.
pub struct UbtEngine {
    inner: Mutex<Inner>,
    probe: Arc<dyn procs::UbtProbe>,
}

struct Inner {
    /// The log to tail; resolved once (overridable in tests).
    log_path: Option<PathBuf>,
    cursor: tail::TailCursor,
    run: Run,
    // EWMA rate tracking.
    rate: Option<f64>,
    last_tick: Instant,
    last_done: u32,
    stall_since: Option<Instant>,
    /// Epoch ms the current run began being tracked (fallback for `started_at`).
    created_ms: i64,
    /// `false` until the tail has drained its initial from-start backlog and reached EOF.
    /// While draining (e.g. attaching to a large in-progress log whose run header is older
    /// than the look-back window, so the tailer reads from 0), we replay historical runs
    /// whose old `Result:` lines would otherwise flash as finished bars (the live file mtime
    /// is fresh). Suppress surfacing until we've caught up to the current run.
    caught_up: bool,
    /// The action the probe streak is counting toward, and how many consecutive new
    /// snapshots have proposed it.
    pending: Option<PendingAction>,
    streak: u8,
    /// `seq` of the last probe snapshot evaluated (a repeated `seq` is not new evidence).
    last_probe_seq: u64,
    /// The latest probe evaluation confirmed the run (a live UBT for its worktree, or the
    /// probe cannot read processes) — grants the long quiet window.
    process_confirmed: bool,
}

impl Default for UbtEngine {
    fn default() -> Self {
        UbtEngine::new(
            ubt::ubt_log_path(),
            Arc::new(procs::SysinfoProbe::default()),
        )
    }
}

impl UbtEngine {
    pub fn new(log_path: Option<PathBuf>, probe: Arc<dyn procs::UbtProbe>) -> UbtEngine {
        UbtEngine {
            inner: Mutex::new(Inner {
                log_path,
                cursor: tail::TailCursor::new(),
                run: Run::default(),
                rate: None,
                last_tick: Instant::now(),
                last_done: 0,
                stall_since: None,
                created_ms: 0,
                caught_up: false,
                pending: None,
                streak: 0,
                last_probe_seq: 0,
                process_confirmed: false,
            }),
            probe,
        }
    }

    /// Advance the engine by tailing the log, and return the current build for its worktree,
    /// if any is surfaceable this tick. `worktrees` is `(name, normalized path)` across all
    /// projects. `now_ms` is the wall clock (epoch ms) used for mtime-based gating.
    pub fn tick(
        &self,
        worktrees: &[(String, String)],
        now_ms: i64,
    ) -> Option<(String, BuildProgress)> {
        let now = Instant::now();
        let mut inner = super::lock(&self.inner);
        let path = inner.log_path.clone()?;

        let read = tail::read_new_lines(&path, &mut inner.cursor).unwrap_or_default();
        if read.reset {
            inner.start_new_run(now_ms);
        }
        for line in &read.lines {
            if line.contains("Log started at") {
                inner.start_new_run(now_ms);
            }
            if !inner.run.dead {
                inner.run.last_line_ms = now_ms;
            }
            let delta = {
                let msg = &mut inner.run.message;
                ubt::parse_line(line, msg)
            };
            if let Some(delta) = delta {
                inner.apply(delta, worktrees, now_ms);
            }
        }
        inner.update_rate(now);

        // Caught up once the cursor has consumed the whole file (steady-state live tailing
        // always ends a tick at EOF; only the initial from-0 replay of a big log lags behind).
        let (len, mtime) = file_len_mtime(&path);
        if inner.cursor.offset >= len {
            inner.caught_up = true;
        }
        if !inner.caught_up {
            return None; // still replaying history; don't flash stale finished runs
        }

        // Process evidence runs after the tail is drained, so a late `Result:` wins. The
        // probe is only asked while a run is live or the log was written recently.
        let log_age_ms = mtime.map_or(i64::MAX, |m| now_ms.saturating_sub(m));
        let live_run = inner.run.attributed && inner.run.finished.is_none() && !inner.run.dead;
        if live_run || log_age_ms <= PROBE_RECENT_LOG_MS {
            let snap = self.probe.snapshot();
            inner.observe(&snap, worktrees, now_ms);
        } else {
            inner.clear_pending();
        }

        let run_age_ms = log_age_ms.max(now_ms.saturating_sub(inner.run.last_line_ms));
        let confirmed = inner.process_confirmed;
        inner.surface(now_ms, run_age_ms, confirmed)
    }
}

impl Inner {
    fn start_new_run(&mut self, now_ms: i64) {
        self.run = Run {
            last_line_ms: now_ms,
            ..Run::default()
        };
        self.rate = None;
        self.last_done = 0;
        self.stall_since = None;
        self.created_ms = now_ms;
        // A streak is evidence about one run; a new run starts counting afresh.
        self.clear_pending();
    }

    /// Drop the current run: nothing is surfaced and later deltas are ignored until a new
    /// run starts.
    fn drop_run(&mut self, now_ms: i64) {
        self.start_new_run(now_ms);
        self.run.dead = true;
    }

    fn clear_pending(&mut self) {
        self.pending = None;
        self.streak = 0;
    }

    /// Evaluate one probe snapshot against the current run. Only a new snapshot counts: a
    /// repeated `seq` changes nothing (not even the streak). A finished run is left alone,
    /// so a `Result:` drained this tick always wins over a missing process.
    fn observe(
        &mut self,
        snap: &procs::ProbeSnapshot,
        worktrees: &[(String, String)],
        now_ms: i64,
    ) {
        if snap.seq == self.last_probe_seq {
            return;
        }
        self.last_probe_seq = snap.seq;
        if self.run.finished.is_some() {
            self.clear_pending();
            return;
        }
        let Some(live) = &snap.procs else {
            // Cannot read processes: never act on absence; keep the long quiet window.
            self.clear_pending();
            self.process_confirmed = true;
            return;
        };

        let mut mapped: BTreeSet<String> = BTreeSet::new();
        let mut unknown = false;
        for p in live {
            match p
                .project
                .as_deref()
                .and_then(|proj| ubt::project_to_worktree(proj, worktrees))
            {
                Some(wt) => {
                    mapped.insert(wt);
                }
                None => unknown = true,
            }
        }
        // A dropped run has no worktree; like an unattributed one it can only be adopted.
        let owner = self
            .run
            .worktree
            .clone()
            .filter(|_| self.run.attributed && !self.run.dead);

        if owner.as_ref().is_some_and(|w| mapped.contains(w)) {
            self.clear_pending();
            self.process_confirmed = true;
            return;
        }
        self.process_confirmed = false;
        if unknown {
            // A UBT we cannot place may be this build: keep the run, but act on nothing.
            self.clear_pending();
            return;
        }
        let candidate = if mapped.len() == 1 {
            mapped.into_iter().next().map(PendingAction::Reattribute)
        } else if owner.is_some() {
            // Nothing builds this worktree, and no single other worktree to move to.
            Some(PendingAction::Cancel)
        } else {
            None // an unattributed run waits
        };
        let Some(candidate) = candidate else {
            self.clear_pending();
            return;
        };

        if self.pending.as_ref() == Some(&candidate) {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.pending = Some(candidate);
            self.streak = 1;
        }
        if self.streak < PROBE_STREAK {
            return;
        }
        match self.pending.take() {
            Some(PendingAction::Cancel) => self.drop_run(now_ms),
            Some(PendingAction::Reattribute(wt)) => {
                // Credit starts from the next step lines of the build actually running.
                self.start_new_run(now_ms);
                self.run.worktree = Some(wt);
                self.run.attributed = true;
                self.process_confirmed = true;
            }
            None => {}
        }
        self.clear_pending();
    }

    /// Apply one parsed delta to the current run.
    fn apply(&mut self, delta: ProgressDelta, worktrees: &[(String, String)], now_ms: i64) {
        if self.run.dead {
            return; // a dropped run is never resurrected by its stragglers
        }
        if let ProgressDelta::Attribute(path) = &delta {
            match ubt::project_to_worktree(path, worktrees) {
                Some(name) => {
                    // A re-attribution to a different worktree (shared log moved to the next
                    // run without a `Log started at` seen) starts fresh.
                    if self.run.worktree.as_deref() != Some(name.as_str()) {
                        let keep_msg = self.run.message.take();
                        self.start_new_run(now_ms);
                        self.run.message = keep_msg;
                    }
                    self.run.worktree = Some(name);
                    self.run.attributed = true;
                }
                None => {
                    // Build for a project not under any registered worktree: don't surface.
                    self.run.worktree = None;
                    self.run.attributed = false;
                }
            }
            return;
        }

        if !self.run.attributed {
            return; // ignore steps/totals/results until attributed
        }

        match delta {
            ProgressDelta::Total(n) => {
                if self.run.finished.is_none() {
                    if n != self.run.total {
                        // A new action batch of the same invocation: its counter restarts.
                        self.run.done = 0;
                    }
                    self.run.total = n;
                    if self.run.started_at == 0 {
                        self.run.started_at = now_ms;
                    }
                }
            }
            ProgressDelta::Step { done, total, phase } => {
                if self.run.finished.is_some() {
                    return; // finished run; ignore trailing repeats
                }
                self.run.done = done;
                if let Some(t) = total {
                    self.run.total = t;
                }
                self.run.phase = phase;
                if self.run.started_at == 0 {
                    self.run.started_at = now_ms;
                }
            }
            ProgressDelta::Succeeded => self.run.finished = Some(BuildState::Succeeded),
            ProgressDelta::Failed(m) => {
                self.run.finished = Some(BuildState::Failed);
                if m.is_some() {
                    self.run.message = m;
                }
            }
            ProgressDelta::Attribute(_) => unreachable!("handled above"),
        }
    }

    /// Update the EWMA action rate from this tick's progress.
    fn update_rate(&mut self, now: Instant) {
        let dt = now.duration_since(self.last_tick).as_secs_f64();
        if dt < MIN_SAMPLE_DT {
            return;
        }
        let done = self.run.done;
        let ddone = i64::from(done) - i64::from(self.last_done);
        if ddone > 0 {
            self.rate = ewma_step(self.rate, ddone, dt, ALPHA);
            self.last_done = done;
            self.stall_since = None;
        } else if ddone == 0 {
            let stall_start = *self.stall_since.get_or_insert(now);
            if now.duration_since(stall_start) > STALL_GRACE {
                self.rate = ewma_step(self.rate, 0, dt, ALPHA);
            }
        } else {
            self.last_done = done;
            self.rate = None;
            self.stall_since = None;
        }
        self.last_tick = now;
    }

    /// Decide whether to surface this run, and build its [`BuildProgress`]. `run_age_ms` is
    /// how long the run has been quiet (the older of its last line and the log's mtime);
    /// `process_confirmed` grants a running run the long quiet window.
    fn surface(
        &self,
        now_ms: i64,
        run_age_ms: i64,
        process_confirmed: bool,
    ) -> Option<(String, BuildProgress)> {
        if !self.run.attributed || self.run.dead {
            return None;
        }
        let wt = self.run.worktree.clone()?;
        let has_progress = self.run.done > 0 || self.run.total > 0;
        let finished = self.run.finished.is_some();
        if !surface_decision(finished, has_progress, run_age_ms, process_confirmed) {
            return None;
        }
        Some((wt, self.to_build_progress(now_ms)))
    }

    fn to_build_progress(&self, now_ms: i64) -> BuildProgress {
        let state = self.run.finished.clone().unwrap_or(BuildState::Running);
        let math = progress_math(self.run.done, self.run.total, &state, self.rate);
        let phase = self.run.phase.clone();
        let label = match &phase {
            Some(p) => format!("UBT · {p}"),
            None => "UBT".to_string(),
        };
        let started_at = if self.run.started_at != 0 {
            self.run.started_at
        } else if self.created_ms != 0 {
            self.created_ms
        } else {
            now_ms
        };
        BuildProgress {
            adapter: "ubt".to_string(),
            label,
            done: self.run.done,
            total: self.run.total,
            phase,
            state,
            started_at,
            eta_secs: math.eta_secs,
            percent: math.percent,
            message: self.run.message.clone(),
        }
    }
}

/// Pure surfacing gate: a finished run shows only while it is recent; a running run shows
/// only with real progress and while it is still being written — five quiet minutes when a
/// live process confirms it, 90 s otherwise.
fn surface_decision(
    finished: bool,
    has_progress: bool,
    run_age_ms: i64,
    process_confirmed: bool,
) -> bool {
    if finished {
        run_age_ms <= FINISHED_RETENTION_MS
    } else {
        let window = if process_confirmed {
            STALE_AFTER_MS
        } else {
            UNCONFIRMED_STALE_AFTER_MS
        };
        has_progress && run_age_ms <= window
    }
}

/// The log file's `(len, mtime_ms)`. A missing/unstatable file is `(0, None)` — length 0 so
/// an absent log counts as "caught up" (nothing to replay) and `None` mtime hides any bar.
fn file_len_mtime(path: &Path) -> (u64, Option<i64>) {
    let Ok(m) = std::fs::metadata(path) else {
        return (0, None);
    };
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    (m.len(), mtime)
}

/// One EWMA step: `rate ← α·(Δdone/Δt) + (1−α)·rate` (first sample seeds `rate`). A
/// negative `ddone` or non-positive `dt` leaves the rate unchanged.
pub fn ewma_step(prev: Option<f64>, ddone: i64, dt: f64, alpha: f64) -> Option<f64> {
    if ddone < 0 || dt <= 0.0 {
        return prev;
    }
    let inst = ddone as f64 / dt;
    Some(match prev {
        None => inst,
        Some(r) => alpha * inst + (1.0 - alpha) * r,
    })
}

/// Pure `%`/ETA computation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProgressMath {
    pub percent: u8,
    pub eta_secs: Option<u32>,
}

/// Compute `percent` (capped) and `eta_secs` (hidden unless meaningful) for a build.
pub fn progress_math(done: u32, total: u32, state: &BuildState, rate: Option<f64>) -> ProgressMath {
    let pct_raw = if total == 0 {
        0
    } else {
        ((u64::from(done) * 100) / u64::from(total)) as u32
    };
    let percent = match state {
        BuildState::Succeeded => 100,
        BuildState::Running | BuildState::Failed => pct_raw.min(99) as u8,
    };

    let eta_secs = if *state == BuildState::Running {
        match rate {
            Some(r) if r > 0.0 && total > 0 && done >= ETA_MIN_DONE => {
                let remaining = total.saturating_sub(done);
                let secs = (f64::from(remaining) / r).ceil();
                if secs.is_finite() && secs >= 0.0 {
                    let s = secs as u64;
                    (s <= ETA_CAP_SECS).then_some(s as u32)
                } else {
                    None
                }
            }
            _ => None,
        }
    } else {
        None
    };

    ProgressMath { percent, eta_secs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::keymap::normalize_either_separator;
    use procs::{FakeProbe, UbtProc};

    // ── progress_math ───────────────────────────────────────────────────────────

    #[test]
    fn percent_caps_at_99_while_running_and_100_on_success() {
        assert_eq!(
            progress_math(130, 130, &BuildState::Running, Some(1.0)).percent,
            99
        );
        assert_eq!(
            progress_math(130, 130, &BuildState::Succeeded, None).percent,
            100
        );
    }

    #[test]
    fn eta_hidden_before_5_actions_present_at_5() {
        assert!(progress_math(4, 100, &BuildState::Running, Some(2.0))
            .eta_secs
            .is_none());
        assert_eq!(
            progress_math(5, 100, &BuildState::Running, Some(2.0)).eta_secs,
            Some(48)
        );
    }

    #[test]
    fn eta_none_when_total_zero_or_no_rate() {
        assert!(progress_math(0, 0, &BuildState::Running, Some(1.0))
            .eta_secs
            .is_none());
        assert!(progress_math(10, 100, &BuildState::Running, None)
            .eta_secs
            .is_none());
    }

    #[test]
    fn eta_hidden_when_finished() {
        assert!(progress_math(130, 130, &BuildState::Succeeded, Some(5.0))
            .eta_secs
            .is_none());
    }

    #[test]
    fn ewma_seeds_then_smooths_and_ignores_bad_samples() {
        let r = ewma_step(None, 10, 2.0, 0.2).unwrap(); // seed → 5.0
        assert!((r - 5.0).abs() < 1e-9);
        let r2 = ewma_step(Some(5.0), 10, 1.0, 0.2).unwrap(); // 0.2*10 + 0.8*5 = 6.0
        assert!((r2 - 6.0).abs() < 1e-9);
        assert_eq!(ewma_step(Some(5.0), -1, 1.0, 0.2), Some(5.0)); // regression ignored
        assert_eq!(ewma_step(Some(5.0), 1, 0.0, 0.2), Some(5.0)); // zero dt ignored
    }

    // ── surface gate ─────────────────────────────────────────────────────────────

    #[test]
    fn surface_gate_running_and_finished_recency() {
        // Finished: shown while recent, hidden once old (confirmation is irrelevant).
        assert!(surface_decision(true, true, 1_000, false));
        assert!(!surface_decision(
            true,
            true,
            FINISHED_RETENTION_MS + 1,
            true
        ));
        // Running: needs progress and a run still being written.
        assert!(surface_decision(false, true, 1_000, false));
        assert!(!surface_decision(false, false, 1_000, true)); // attributed but no progress yet
        assert!(!surface_decision(false, true, STALE_AFTER_MS + 1, true)); // gone quiet → stale
    }

    #[test]
    fn surface_gate_quiet_window_depends_on_process_confirmation() {
        let between = UNCONFIRMED_STALE_AFTER_MS + 1;
        assert!(between <= STALE_AFTER_MS);
        // A live matching process keeps a quiet (long link) run for five minutes...
        assert!(surface_decision(false, true, between, true));
        assert!(surface_decision(false, true, STALE_AFTER_MS, true));
        // ...without one, 90 s.
        assert!(surface_decision(
            false,
            true,
            UNCONFIRMED_STALE_AFTER_MS,
            false
        ));
        assert!(!surface_decision(false, true, between, false));
    }

    // ── engine over the fixture logs ───────────────────────────────────────────────

    fn unavailable() -> Arc<dyn procs::UbtProbe> {
        Arc::new(FakeProbe::unavailable())
    }

    fn write_log(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Log.txt");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn engine_tracks_success_fixture_and_attributes_to_worktree() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_success.log"));
        let engine = UbtEngine::new(Some(path), unavailable());
        let worktrees = vec![(
            "alpha".to_string(),
            normalize_either_separator(r"D:\repos\proj-worktrees\alpha"),
        )];
        let now_ms = crate::activity::workflow::now_ms();
        let (wt, bp) = engine.tick(&worktrees, now_ms).expect("surfaced");
        assert_eq!(wt, "alpha");
        assert_eq!(bp.total, 130);
        assert_eq!(
            bp.done, 129,
            "last real step; [130/130] WriteMetadata is [NoUba]"
        );
        assert_eq!(bp.state, BuildState::Succeeded);
        assert_eq!(bp.percent, 100);
        assert_eq!(bp.adapter, "ubt");
    }

    #[test]
    fn engine_tracks_failure_fixture_with_message() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_failure.log"));
        let engine = UbtEngine::new(Some(path), unavailable());
        let worktrees = vec![(
            "beta".to_string(),
            normalize_either_separator(r"D:\repos\proj-worktrees\beta"),
        )];
        let now_ms = crate::activity::workflow::now_ms();
        let (wt, bp) = engine.tick(&worktrees, now_ms).expect("surfaced");
        assert_eq!(wt, "beta");
        assert_eq!(bp.state, BuildState::Failed);
        assert!(bp.message.unwrap().contains("error C2065"));
    }

    #[test]
    fn engine_drops_build_for_unregistered_worktree() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_success.log"));
        let engine = UbtEngine::new(Some(path), unavailable());
        // alpha is not in the registered set → nothing to attribute to.
        let worktrees = vec![(
            "other".to_string(),
            normalize_either_separator(r"D:\repos\other"),
        )];
        let now_ms = crate::activity::workflow::now_ms();
        assert!(engine.tick(&worktrees, now_ms).is_none());
    }

    #[test]
    fn engine_hides_old_finished_run_by_mtime() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_success.log"));
        let engine = UbtEngine::new(Some(path), unavailable());
        let worktrees = vec![(
            "alpha".to_string(),
            normalize_either_separator(r"D:\repos\proj-worktrees\alpha"),
        )];
        // Pretend "now" is far in the future, so the log's mtime is well past retention.
        let future = crate::activity::workflow::now_ms() + FINISHED_RETENTION_MS + 60_000;
        assert!(
            engine.tick(&worktrees, future).is_none(),
            "a long-finished run must not paint a bar"
        );
    }

    #[test]
    fn engine_missing_log_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let engine = UbtEngine::new(Some(dir.path().join("does-not-exist.txt")), unavailable());
        let now_ms = crate::activity::workflow::now_ms();
        assert!(engine.tick(&[], now_ms).is_none());
    }

    #[test]
    fn engine_does_not_flash_old_runs_while_replaying_a_large_log() {
        // A shared log holding an old finished run (beta) followed by a current, still-running
        // run (alpha) whose header is far enough back that the initial read drains over more
        // than one tick. The first tick (still replaying history) must surface nothing rather
        // than flash beta's stale "Succeeded"; once caught up, alpha's running bar shows.
        let mut log = String::new();
        log.push_str("Log started at 10/02/2026 10:00:00\n");
        log.push_str(
            "Resolved project argument D:\\repos\\proj-worktrees\\beta\\F.uproject to x\n",
        );
        log.push_str("Using local executor to run 2 action(s)\n");
        log.push_str("[1/2] Compile [x64] a.cpp\n[2/2] Link [x64] b.dll\n");
        log.push_str("Result: Succeeded\n");
        log.push_str("Log started at 10/02/2026 11:00:00\n");
        log.push_str(
            "Resolved project argument D:\\repos\\proj-worktrees\\alpha\\F.uproject to x\n",
        );
        log.push_str("Using local executor to run 99999 action(s)\n");
        // >1 MiB of step lines so the from-0 replay needs more than one 1-MiB tick.
        for i in 1..=30_000u32 {
            log.push_str(&format!("[{i}/99999] Compile [x64] File{i}.cpp\n"));
        }
        assert!(
            log.len() as u64 > 1024 * 1024,
            "log must exceed one tick's read cap"
        );

        let (_dir, path) = write_log(&log);
        let engine = UbtEngine::new(Some(path), unavailable());
        let worktrees = vec![
            (
                "beta".to_string(),
                normalize_either_separator(r"D:\repos\proj-worktrees\beta"),
            ),
            (
                "alpha".to_string(),
                normalize_either_separator(r"D:\repos\proj-worktrees\alpha"),
            ),
        ];
        let now_ms = crate::activity::workflow::now_ms();

        // First tick: still draining the backlog → nothing surfaced (no beta "Succeeded" flash).
        assert!(
            engine.tick(&worktrees, now_ms).is_none(),
            "must not flash an old finished run mid-replay"
        );
        // Subsequent ticks catch up to the current run.
        let mut surfaced = None;
        for _ in 0..4 {
            if let Some(got) = engine.tick(&worktrees, now_ms) {
                surfaced = Some(got);
                break;
            }
        }
        let (wt, bp) = surfaced.expect("current run surfaces once caught up");
        assert_eq!(wt, "alpha", "the current run, not the old beta run");
        assert_eq!(bp.state, BuildState::Running);
    }

    // ── engine with process evidence ───────────────────────────────────────────────

    fn wt_path(name: &str) -> String {
        normalize_either_separator(&format!(r"D:\repos\proj-worktrees\{name}"))
    }

    fn all_worktrees() -> Vec<(String, String)> {
        ["alpha", "beta", "gamma"]
            .iter()
            .map(|n| (n.to_string(), wt_path(n)))
            .collect()
    }

    /// A live build-mode UBT for worktree `name`.
    fn ubt_for(name: &str, pid: u32) -> UbtProc {
        UbtProc {
            pid,
            project: Some(format!("{}/MyGame.uproject", wt_path(name))),
        }
    }

    /// A live build-mode UBT whose project could not be read.
    fn unknown_ubt(pid: u32) -> UbtProc {
        UbtProc { pid, project: None }
    }

    fn engine_with(path: PathBuf, probe: &Arc<FakeProbe>) -> UbtEngine {
        UbtEngine::new(Some(path), probe.clone())
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    /// An alpha build in progress: `[5/100]`, no `Result:`.
    fn running_alpha_log() -> String {
        let mut log = String::from("Log started at 01/01/2026 12:00:00\n");
        log.push_str(
            "Resolved project argument D:\\repos\\proj-worktrees\\alpha\\MyGame.uproject to x\n",
        );
        log.push_str("Using local executor to run 100 action(s)\n");
        for i in 1..=5 {
            log.push_str(&format!("[{i}/100] Compile [x64] File{i}.cpp\n"));
        }
        log
    }

    fn surfaced_wt(got: &Option<(String, BuildProgress)>) -> Option<&str> {
        got.as_ref().map(|(wt, _)| wt.as_str())
    }

    #[test]
    fn cancelled_run_clears_after_two_new_snapshots_without_the_process() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_cancelled.log"));
        let probe = Arc::new(FakeProbe::live(vec![ubt_for("alpha", 10)]));
        let engine = engine_with(path, &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();

        let (wt, bp) = engine.tick(&wts, now).expect("running alpha build");
        assert_eq!((wt.as_str(), bp.done, bp.total), ("alpha", 8, 60));
        assert_eq!(bp.state, BuildState::Running);

        // The process is gone: one new snapshot is not enough...
        probe.set(Some(Vec::new()));
        assert_eq!(surfaced_wt(&engine.tick(&wts, now)), Some("alpha"));
        // ...and the same snapshot again is not new evidence.
        assert_eq!(surfaced_wt(&engine.tick(&wts, now)), Some("alpha"));
        // A second new snapshot drops the run: no bar, so the lane is no longer `working`
        // (which needs a surfaced Running build).
        probe.bump();
        assert!(engine.tick(&wts, now).is_none());
        probe.bump();
        assert!(engine.tick(&wts, now).is_none(), "stays gone");
    }

    #[test]
    fn a_confirming_snapshot_breaks_the_streak() {
        let (_dir, path) = write_log(&running_alpha_log());
        let probe = Arc::new(FakeProbe::live(vec![ubt_for("alpha", 10)]));
        let engine = engine_with(path, &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        assert!(engine.tick(&wts, now).is_some());

        probe.set(Some(Vec::new()));
        assert!(engine.tick(&wts, now).is_some());
        probe.set(Some(vec![ubt_for("alpha", 10)]));
        assert!(engine.tick(&wts, now).is_some());
        probe.set(Some(Vec::new()));
        assert_eq!(
            surfaced_wt(&engine.tick(&wts, now)),
            Some("alpha"),
            "the miss streak restarted after the confirmation"
        );
    }

    #[test]
    fn late_result_wins_over_a_missing_process() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_cancelled.log"));
        let probe = Arc::new(FakeProbe::live(vec![ubt_for("alpha", 10)]));
        let engine = engine_with(path.clone(), &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        assert!(engine.tick(&wts, now).is_some());

        probe.set(Some(Vec::new()));
        assert!(engine.tick(&wts, now).is_some());
        // The process exited after writing its result; the tail is drained first.
        append(&path, "Result: Succeeded\n");
        probe.bump();
        let (wt, bp) = engine.tick(&wts, now).expect("the result is shown");
        assert_eq!(wt, "alpha");
        assert_eq!(bp.state, BuildState::Succeeded);
    }

    #[test]
    fn cancelled_build_then_rotated_longer_log_shows_the_new_worktree_only() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_rotation_first.log"));
        let probe = Arc::new(FakeProbe::live(vec![ubt_for("alpha", 10)]));
        let engine = engine_with(path.clone(), &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        assert_eq!(surfaced_wt(&engine.tick(&wts, now)), Some("alpha"));

        // Alpha is cancelled.
        probe.set(Some(Vec::new()));
        engine.tick(&wts, now);
        probe.bump();
        assert!(engine.tick(&wts, now).is_none());

        // Beta starts: UBT rotates the log, and the new file is already longer than the
        // old offset by the next poll.
        let second = include_str!("fixtures/ubt_rotation_second.log");
        assert!(second.len() > include_str!("fixtures/ubt_rotation_first.log").len());
        std::fs::write(&path, second).unwrap();
        probe.set(Some(vec![ubt_for("beta", 11)]));
        for _ in 0..3 {
            let (wt, bp) = engine.tick(&wts, now).expect("beta's build");
            assert_eq!((wt.as_str(), bp.done, bp.total), ("beta", 12, 25));
            probe.bump();
        }
    }

    #[test]
    fn steps_from_a_build_with_a_missed_header_move_to_the_probed_worktree() {
        let (_dir, path) = write_log(&running_alpha_log());
        let probe = Arc::new(FakeProbe::live(vec![ubt_for("alpha", 10)]));
        let engine = engine_with(path.clone(), &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        let (_, bp) = engine.tick(&wts, now).expect("alpha");
        assert_eq!((bp.done, bp.total), (5, 100));

        // Beta's steps land in the tail without its header or attribution lines, and alpha
        // is gone: the probe moves the run within two snapshots.
        append(
            &path,
            "[1/40] Compile [x64] B1.cpp\n[2/40] Compile [x64] B2.cpp\n",
        );
        probe.set(Some(vec![ubt_for("beta", 11)]));
        engine.tick(&wts, now);
        probe.bump();
        assert!(
            engine.tick(&wts, now).is_none(),
            "reattributed to beta, no progress yet"
        );

        append(&path, "[3/40] Compile [x64] B3.cpp\n");
        let (wt, bp) = engine.tick(&wts, now).expect("beta's progress");
        assert_eq!((wt.as_str(), bp.done, bp.total), ("beta", 3, 40));
    }

    #[test]
    fn a_new_action_batch_stays_in_the_same_attributed_run() {
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();

        // A step whose total differs.
        let (_dir, path) = write_log(&running_alpha_log());
        let engine = UbtEngine::new(Some(path.clone()), unavailable());
        assert!(engine.tick(&wts, now).is_some());
        append(
            &path,
            "[1/250] Compile [x64] Other.cpp\n[2/250] Compile [x64] Other2.cpp\n",
        );
        let (wt, bp) = engine.tick(&wts, now).expect("same run");
        assert_eq!((wt.as_str(), bp.done, bp.total), ("alpha", 2, 250));

        // A new action total.
        let (_dir2, path2) = write_log(&running_alpha_log());
        let engine = UbtEngine::new(Some(path2.clone()), unavailable());
        assert!(engine.tick(&wts, now).is_some());
        append(&path2, "Using local executor to run 300 action(s)\n");
        let (wt, bp) = engine.tick(&wts, now).expect("same run");
        assert_eq!((wt.as_str(), bp.done, bp.total), ("alpha", 0, 300));

        // A step count going backwards.
        let (_dir3, path3) = write_log(&running_alpha_log());
        let engine = UbtEngine::new(Some(path3.clone()), unavailable());
        assert!(engine.tick(&wts, now).is_some());
        append(&path3, "[2/100] Compile [x64] Again.cpp\n");
        let (wt, bp) = engine.tick(&wts, now).expect("same run");
        assert_eq!((wt.as_str(), bp.done, bp.total), ("alpha", 2, 100));
    }

    #[test]
    fn a_build_whose_project_maps_to_no_worktree_is_unknown_and_keeps_the_run() {
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        let unmapped = |project: &str| UbtProc {
            pid: 10,
            project: Some(project.to_string()),
        };
        for live in [
            // Relative, short-form, and foreign-root project paths.
            vec![unmapped("MyGame.uproject")],
            vec![unmapped("D:/REPOS/PROJ-W~1/alpha/MyGame.uproject")],
            vec![unmapped("E:/elsewhere/MyGame.uproject")],
            // Unknown alongside a mapped other worktree: still never reattributed.
            vec![unmapped("MyGame.uproject"), ubt_for("beta", 11)],
        ] {
            let (_dir, path) = write_log(&running_alpha_log());
            let probe = Arc::new(FakeProbe::live(live));
            let engine = engine_with(path, &probe);
            for _ in 0..6 {
                assert_eq!(surfaced_wt(&engine.tick(&wts, now)), Some("alpha"));
                probe.bump();
            }
        }
    }

    #[test]
    fn a_mapped_other_worktree_still_reattributes_when_nothing_is_unknown() {
        let (_dir, path) = write_log(&running_alpha_log());
        let other = UbtProc {
            pid: 11,
            project: Some(format!(
                "{}/MyGame.uproject",
                wt_path("beta").to_uppercase()
            )),
        };
        let probe = Arc::new(FakeProbe::live(vec![other]));
        let engine = engine_with(path.clone(), &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        assert_eq!(surfaced_wt(&engine.tick(&wts, now)), Some("alpha"));
        probe.bump();
        assert!(engine.tick(&wts, now).is_none());
        append(&path, "[1/20] Compile [x64] B1.cpp\n");
        assert_eq!(surfaced_wt(&engine.tick(&wts, now)), Some("beta"));
    }

    #[test]
    fn an_unknown_live_ubt_keeps_the_run_but_does_not_extend_its_window() {
        let (_dir, path) = write_log(&running_alpha_log());
        let probe = Arc::new(FakeProbe::live(vec![unknown_ubt(10), ubt_for("beta", 11)]));
        let engine = engine_with(path, &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        for _ in 0..4 {
            assert_eq!(
                surfaced_wt(&engine.tick(&wts, now)),
                Some("alpha"),
                "never cancelled or reattributed while an unknown UBT is live"
            );
            probe.bump();
        }
        // Not process-confirmed: the quiet window is 90 s, not five minutes.
        assert_eq!(surfaced_wt(&engine.tick(&wts, now + 60_000)), Some("alpha"));
        probe.bump();
        assert!(engine
            .tick(&wts, now + UNCONFIRMED_STALE_AFTER_MS + 10_000)
            .is_none());
    }

    #[test]
    fn a_dropped_run_is_not_resurrected_by_later_lines() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_cancelled.log"));
        let probe = Arc::new(FakeProbe::live(vec![ubt_for("alpha", 10)]));
        let engine = engine_with(path.clone(), &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        assert!(engine.tick(&wts, now).is_some());
        probe.set(Some(Vec::new()));
        engine.tick(&wts, now);
        probe.bump();
        assert!(engine.tick(&wts, now).is_none());

        // Stragglers, including an attribution line and a result, revive nothing.
        append(
            &path,
            "Resolved project argument D:\\repos\\proj-worktrees\\alpha\\MyGame.uproject to x\n\
             [9/60] Compile [x64] Late.cpp\nResult: Succeeded\n",
        );
        probe.bump();
        assert!(engine.tick(&wts, now).is_none());
        probe.bump();
        assert!(engine.tick(&wts, now).is_none());

        // Only a new run header starts tracking again.
        append(
            &path,
            "Log started at 01/01/2026 12:30:00\n\
             Resolved project argument D:\\repos\\proj-worktrees\\alpha\\MyGame.uproject to x\n\
             [1/10] Compile [x64] Next.cpp\n",
        );
        probe.set(Some(vec![ubt_for("alpha", 12)]));
        let (wt, bp) = engine.tick(&wts, now).expect("the new run");
        assert_eq!((wt.as_str(), bp.done, bp.total), ("alpha", 1, 10));
    }

    #[test]
    fn an_unavailable_probe_never_cancels_and_keeps_the_five_minute_window() {
        let (_dir, path) = write_log(&running_alpha_log());
        let probe = Arc::new(FakeProbe::unavailable());
        let engine = engine_with(path, &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        for _ in 0..4 {
            assert_eq!(surfaced_wt(&engine.tick(&wts, now)), Some("alpha"));
            probe.bump();
        }
        // A long quiet link step: still shown past 90 s...
        assert_eq!(
            surfaced_wt(&engine.tick(&wts, now + UNCONFIRMED_STALE_AFTER_MS + 60_000)),
            Some("alpha")
        );
        // ...until the five-minute staleness.
        probe.bump();
        assert!(engine.tick(&wts, now + STALE_AFTER_MS + 10_000).is_none());
    }

    #[test]
    fn exactly_one_other_live_worktree_takes_over_the_run() {
        let (_dir, path) = write_log(&running_alpha_log());
        let probe = Arc::new(FakeProbe::live(vec![ubt_for("beta", 11)]));
        let engine = engine_with(path.clone(), &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();

        assert_eq!(
            surfaced_wt(&engine.tick(&wts, now)),
            Some("alpha"),
            "one snapshot"
        );
        probe.bump();
        assert!(
            engine.tick(&wts, now).is_none(),
            "moved to beta, which has no progress yet; never left on alpha"
        );
        append(&path, "[1/20] Compile [x64] B1.cpp\n");
        let (wt, bp) = engine.tick(&wts, now).expect("beta's progress");
        assert_eq!((wt.as_str(), bp.done, bp.total), ("beta", 1, 20));
    }

    #[test]
    fn two_other_live_worktrees_drop_the_run_without_reattributing() {
        let (_dir, path) = write_log(&running_alpha_log());
        let probe = Arc::new(FakeProbe::live(vec![
            ubt_for("beta", 11),
            ubt_for("gamma", 12),
        ]));
        let engine = engine_with(path.clone(), &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();

        assert!(engine.tick(&wts, now).is_some());
        probe.bump();
        assert!(engine.tick(&wts, now).is_none());
        for _ in 0..3 {
            probe.bump();
            assert!(
                engine.tick(&wts, now).is_none(),
                "no worktree adopts the run"
            );
        }
        append(&path, "[6/100] Compile [x64] File6.cpp\n");
        assert!(engine.tick(&wts, now).is_none());
    }

    #[test]
    fn an_unknown_ubt_blocks_both_reattribution_and_cancellation() {
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();
        for live in [
            vec![ubt_for("beta", 11), unknown_ubt(13)],
            vec![ubt_for("beta", 11), ubt_for("gamma", 12), unknown_ubt(13)],
            vec![unknown_ubt(13)],
        ] {
            let (_dir, path) = write_log(&running_alpha_log());
            let probe = Arc::new(FakeProbe::live(live));
            let engine = engine_with(path, &probe);
            for _ in 0..4 {
                assert_eq!(surfaced_wt(&engine.tick(&wts, now)), Some("alpha"));
                probe.bump();
            }
        }
    }

    #[test]
    fn an_unattributed_run_is_adopted_only_by_a_single_mapped_build() {
        // A run header with no attribution line.
        let mut log = String::from("Log started at 01/01/2026 12:00:00\n");
        log.push_str("Using local executor to run 10 action(s)\n");
        log.push_str("[1/10] Compile [x64] A.cpp\n[2/10] Compile [x64] B.cpp\n");
        let (_dir, path) = write_log(&log);
        let probe = Arc::new(FakeProbe::live(vec![ubt_for("alpha", 10), unknown_ubt(11)]));
        let engine = engine_with(path.clone(), &probe);
        let wts = all_worktrees();
        let now = crate::activity::workflow::now_ms();

        for live in [
            vec![ubt_for("alpha", 10), unknown_ubt(11)],
            vec![ubt_for("alpha", 10), ubt_for("beta", 11)],
            Vec::new(),
        ] {
            probe.set(Some(live));
            for _ in 0..3 {
                assert!(engine.tick(&wts, now).is_none());
                probe.bump();
            }
        }

        probe.set(Some(vec![ubt_for("alpha", 10)]));
        assert!(engine.tick(&wts, now).is_none());
        probe.bump();
        assert!(engine.tick(&wts, now).is_none(), "adopted, no progress yet");
        append(&path, "[3/10] Compile [x64] C.cpp\n");
        let (wt, bp) = engine.tick(&wts, now).expect("alpha's progress");
        assert_eq!((wt.as_str(), bp.done, bp.total), ("alpha", 3, 10));
    }

    #[test]
    fn a_long_quiet_running_log_at_startup_shows_nothing() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_cancelled.log"));
        let engine = UbtEngine::new(Some(path), unavailable());
        let future = crate::activity::workflow::now_ms() + STALE_AFTER_MS + 60_000;
        assert!(engine.tick(&all_worktrees(), future).is_none());
    }

    #[test]
    fn the_probe_is_not_asked_without_a_live_run_or_a_recent_log() {
        let (_dir, path) = write_log(include_str!("fixtures/ubt_success.log"));
        let probe = Arc::new(FakeProbe::unavailable());
        let engine = engine_with(path, &probe);
        let wts = vec![("alpha".to_string(), wt_path("alpha"))];
        // Finished run, log written long ago.
        let future = crate::activity::workflow::now_ms() + PROBE_RECENT_LOG_MS + 60_000;
        assert!(engine.tick(&wts, future).is_none());
        assert_eq!(probe.calls(), 0);
        // A freshly written log is worth a look (a new build may be starting).
        engine.tick(&wts, crate::activity::workflow::now_ms());
        assert_eq!(probe.calls(), 1);
    }
}

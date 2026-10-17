//! `RecoveryMachine` — the pure recovery policy.
//!
//! After the herdr server dies the machine decides when to restart it (backoff,
//! crash-loop breaker, a sustained memory-headroom check), then reconciles and
//! resumes sessions a few at a time (a bounded in-flight set) with a memory check
//! sized for every session in flight, falling back to one at a time with a
//! stagger once resumes start failing.
//! It never sleeps, samples memory, reads the clock or touches herdr: time and
//! headroom readings enter only as arguments and events, and every effect leaves
//! as a [`Command`] for the driver to execute.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use keitora_tasks::RecoveryConfig;

use crate::backend::SessionKey;
use crate::headroom::{self, HeadroomSnapshot, MemoryPressure, PHYS_MARGIN_BYTES};
use crate::lifecycle::ServerStartHow;

use super::payload::{self, sessions, QueueItem, RecoveryPayload, SessionRef, Situation};
use crate::backend::herdr::ops::{Generation, ServerInstance};
use crate::backend::herdr::reconcile::ReconcileOutcome;
use crate::backend::herdr::watchdog::DeathEvent;

/// Gaps between headroom samples while waiting; the last value repeats.
pub const CHECK_GAPS_SECS: [u64; 7] = [5, 5, 10, 10, 15, 15, 30];
/// How long a restart may take before it counts as a failed start.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long reconciling may take before recovery pauses.
pub const RECONCILE_TIMEOUT: Duration = Duration::from_secs(60);
/// Consecutive failed starts that pause recovery.
pub const MAX_START_FAILURES: u32 = 3;

/// Where server recovery stands. Serialized by variant name on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum Phase {
    Healthy,
    Down,
    AwaitHeadroom,
    Restarting,
    Reconciling,
    Resuming,
    Paused,
}

impl Phase {
    /// The snake_case name lifecycle records use.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Healthy => "healthy",
            Phase::Down => "down",
            Phase::AwaitHeadroom => "await_headroom",
            Phase::Restarting => "restarting",
            Phase::Reconciling => "reconciling",
            Phase::Resuming => "resuming",
            Phase::Paused => "paused",
        }
    }
}

/// Why a death happened, as far as the evidence tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeathClass {
    /// The machine was short of memory when the server died.
    MemoryPressure,
    /// The server died soon after it started.
    EarlyDeath,
    Unknown,
}

/// Why recovery stopped and waits for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    /// Too many deaths inside the breaker window; the class of the last one.
    CrashLoop(DeathClass),
    /// A death soon after the user pressed "Resume now".
    ResumeRetrip(DeathClass),
    /// Memory never became sufficient within the cap.
    HeadroomCap,
    StartFailed,
    ReconcileFailed,
    /// The driver itself failed.
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryState {
    Queued,
    /// In flight (its key is in the machine's in-flight set).
    Resuming,
    /// Not resumed automatically for now (see [`SuspendCause`]).
    Suspended,
}

/// Why a suspended entry waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendCause {
    /// The memory check failed mid-queue; resumes when memory recovers.
    Gate,
    /// The halved auto-resume budget ran out; resumes on focus, "Resume all" or
    /// the healthy reset.
    Budget,
}

/// One session owed a resume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEntry {
    pub key: SessionKey,
    pub generation: Generation,
    pub state: EntryState,
    pub cause: Option<SuspendCause>,
    /// The session was mid-turn when the server died.
    pub busy_at_death: bool,
    /// Promoted by the user: ignores the memory check and the budget.
    pub force: bool,
    /// Start a fresh conversation instead of resuming the stored one.
    pub fresh: bool,
    /// Cancelled while in flight: its result is discarded.
    pub cancelled: bool,
    /// The attempt id of the resume last started for this entry (0: none yet).
    /// Only that attempt's result is accepted.
    pub attempt: u64,
}

/// How a resume launches the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeMode {
    /// The stored decision (resume / continue / fresh, as the resume store says).
    Stored,
    /// Forget the stored conversation and start fresh.
    Fresh,
}

/// How one resume ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeOutcomeKind {
    Resumed,
    Continued,
    StartedFresh,
    Failed(String),
    /// The user started the session again before its turn: nothing was done,
    /// and it counts neither as resumed nor as failed.
    AlreadyRunning,
    /// The user terminated the session before its turn: nothing was done. It
    /// leaves the run without counting as resumed or failed, and unlike
    /// [`Self::AlreadyRunning`] it is not settled (nothing runs).
    Cancelled,
}

/// One live session as last seen while the server was up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub key: SessionKey,
    /// The agent is working or blocked on a prompt (mid-turn).
    pub busy: bool,
}

/// Everything the machine reacts to.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Boot classification. `live = Some(i)`: a different server instance is
    /// already up and only needs reconciling. `bypass_gate`: nothing is at risk,
    /// start at once without waiting for memory. `owed`: how many sessions the
    /// launch owes a resume (0 when unknown or irrelevant); memory ample for all
    /// of them skips the sustained wait.
    BootRecovery {
        generation: Generation,
        live: Option<ServerInstance>,
        bypass_gate: bool,
        owed: u32,
    },
    /// The server died. `generation` is the value after the backend bumped it for
    /// this death, distinct from the pre-bump generation the death report itself
    /// was stamped with.
    Death {
        ev: DeathEvent,
        generation: Generation,
    },
    /// Answer to [`Command::SampleHeadroom`] (`None` = probe unavailable).
    HeadroomTick(Option<HeadroomSnapshot>),
    /// A timer the driver waited for expired; the machine checks its deadlines.
    TimerFired,
    /// Answer to [`Command::StartServer`].
    ServerStarted(ServerInstance),
    ServerStartFailed(String),
    /// Answer to [`Command::Reconcile`].
    ReconcileDone(Result<ReconcileOutcome, String>),
    /// Answer to [`Command::Resume`], carrying its `attempt` back.
    /// `load_related`: the resume timed out or its agent exited early (the
    /// machine is likely overloaded).
    ResumeResult {
        key: SessionKey,
        generation: Generation,
        attempt: u64,
        outcome: ResumeOutcomeKind,
        load_related: bool,
    },
    /// Every live session, from the backend's session listing.
    Observed(Vec<Observation>),
    /// The banner's "Resume now" / "Resume all".
    UserResumeNow,
    /// Settings → Restart: stop, start, reconcile, resume; bypasses the breaker.
    UserRestart {
        generation: Generation,
    },
    /// A suspended pane became active or its "Resume" button was clicked.
    UserFocus(SessionKey),
    /// "Start fresh conversation" on a recovering or suspended pane.
    UserStartFresh(SessionKey),
    /// The session was killed or its worktree removed.
    Cancel(SessionKey),
    /// The project was removed.
    CancelProject(String),
    /// A transfer holds (`true`) or released (`false`) the key.
    TransferLock(SessionKey, bool),
    /// Settings → Stop.
    ExplicitStop,
    /// The driver failed.
    DriverFault,
    /// The backend is going away; the driver stops (the machine ignores it).
    Shutdown,
}

/// What a notification announces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotifyKind {
    /// The first death of an episode; `live` sessions were running.
    Died { live: u32 },
    /// The first failing memory check of an episode.
    AwaitingMemory,
    /// Recovery finished.
    Done {
        resumed: u32,
        mid_turn: u32,
        failed: u32,
        suspended: u32,
    },
    /// Recovery paused; `reason` is the paused sentence.
    Paused { reason: String },
}

/// Everything the machine asks the world to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Sample headroom and answer with [`Event::HeadroomTick`] in the same turn.
    SampleHeadroom,
    /// Start the server (stopping the live one first when `stop_first`).
    StartServer {
        stop_first: bool,
        how: ServerStartHow,
    },
    Reconcile {
        generation: Generation,
        observed: ServerInstance,
    },
    /// Resume `key`. `attempt` is unique per started resume and comes back in
    /// its [`Event::ResumeResult`]: a late result of an earlier attempt of the
    /// same key (one a pause took back) is told apart from the current one.
    Resume {
        key: SessionKey,
        generation: Generation,
        mode: ResumeMode,
        attempt: u64,
    },
    Notify(NotifyKind),
    /// Emit `daemon://recovery` with a fresh snapshot.
    EmitRecovery,
    /// Log a phase transition.
    Record {
        from: Phase,
        to: Phase,
        reason: String,
    },
    /// Log how long a finished recovery took.
    RecordTiming {
        /// From the first memory wait to the start it allowed (0: no wait).
        sustain_ms: u64,
        /// From the first resume phase to the finish.
        resume_ms: u64,
        sessions: u32,
        failed: u32,
        /// The concurrency cap at the finish.
        final_cap: u32,
    },
}

/// The machine's plain-data view of `[pty.herdr.recovery]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryParams {
    /// Free commit (bytes) that must remain after the next session starts.
    pub base_margin: u64,
    /// Estimated cost of one more session (bytes), against commit and physical.
    pub per_session: u64,
    pub sustain: Duration,
    pub headroom_cap: Duration,
    pub stagger: Duration,
    /// How many sessions may resume at the same time. At least 1.
    pub resume_concurrency: u32,
    /// Never empty.
    pub backoff: Vec<Duration>,
    /// At least 1.
    pub breaker_deaths: u32,
    pub breaker_window: Duration,
    pub early_death: Duration,
    pub resume_retrip: Duration,
    pub healthy_reset: Duration,
}

impl RecoveryParams {
    pub fn from_config(cfg: &RecoveryConfig) -> Self {
        let c = cfg.normalized();
        let secs = Duration::from_secs;
        RecoveryParams {
            base_margin: c.base_margin_bytes(),
            per_session: c.per_session_bytes(),
            sustain: secs(c.headroom_sustain_secs),
            headroom_cap: secs(c.headroom_cap_secs),
            stagger: secs(c.stagger_secs),
            resume_concurrency: c.resume_concurrency,
            backoff: c.backoff_secs.iter().copied().map(secs).collect(),
            breaker_deaths: c.breaker_deaths,
            breaker_window: secs(c.breaker_window_secs),
            early_death: secs(c.early_death_secs),
            resume_retrip: secs(c.resume_retrip_secs),
            healthy_reset: secs(c.healthy_reset_secs),
        }
    }
}

impl Default for RecoveryParams {
    fn default() -> Self {
        Self::from_config(&RecoveryConfig::default())
    }
}

/// The memory gate for `sessions` sessions running at once: enough commit for
/// the margin plus their cost, enough physical memory for their cost plus the
/// physical margin, and no OS pressure warning. A missing snapshot (probe
/// unavailable) passes: a broken probe must degrade to "no gate", never to a
/// permanent stall.
pub fn passes(p: &RecoveryParams, snap: Option<&HeadroomSnapshot>, sessions: u32) -> bool {
    match snap {
        None => true,
        Some(s) => headroom::meets(
            s,
            headroom::required_bytes(p.base_margin, p.per_session, sessions),
            headroom::required_phys_bytes(p.per_session, sessions),
        ),
    }
}

/// Whole milliseconds, saturating.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Classify a death; the first match wins: memory pressure at death, then an
/// early death, then unknown.
pub fn classify(ev: &DeathEvent, p: &RecoveryParams) -> DeathClass {
    if let Some(h) = &ev.headroom_at_death {
        let pressured = matches!(
            h.pressure,
            Some(MemoryPressure::Warn | MemoryPressure::Critical)
        );
        if h.commit_avail < p.base_margin || h.phys_avail < PHYS_MARGIN_BYTES || pressured {
            return DeathClass::MemoryPressure;
        }
    }
    if ev.uptime < p.early_death {
        return DeathClass::EarlyDeath;
    }
    DeathClass::Unknown
}

/// `(title, body)` of a notification.
pub fn render(kind: &NotifyKind) -> (String, String) {
    match kind {
        NotifyKind::Died { live } => {
            let body = if *live > 0 {
                format!("{} will restart automatically.", sessions(u64::from(*live)))
            } else {
                "Restarting automatically.".to_string()
            };
            ("herdr stopped".to_string(), body)
        }
        NotifyKind::AwaitingMemory => (
            "Waiting for memory".to_string(),
            "The system is low on memory. Sessions will resume once it frees up.".to_string(),
        ),
        NotifyKind::Done {
            resumed,
            mid_turn,
            failed,
            suspended,
        } => {
            let title = if *failed > 0 {
                "Recovery finished with problems"
            } else {
                "Sessions resumed"
            };
            if *resumed == 0 && *mid_turn == 0 && *failed == 0 && *suspended == 0 {
                return (
                    title.to_string(),
                    "herdr restarted. No sessions needed resuming.".to_string(),
                );
            }
            let mut body = format!("Resumed {}", sessions(u64::from(*resumed)));
            if *mid_turn > 0 {
                let verb = if *mid_turn == 1 { "was" } else { "were" };
                body.push_str(&format!("; {mid_turn} {verb} mid-turn and lost that turn"));
            }
            if *failed > 0 {
                body.push_str(&format!("; {failed} could not be resumed"));
            }
            if *suspended > 0 {
                let obj = if *suspended == 1 { "it" } else { "them" };
                body.push_str(&format!(
                    "; {suspended} suspended \u{2014} open Keitora to resume {obj}"
                ));
            }
            body.push('.');
            (title.to_string(), body)
        }
        NotifyKind::Paused { reason } => (
            "Recovery paused".to_string(),
            format!("{reason} Open Keitora to resume."),
        ),
    }
}

/// What the machine last knew about a live session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LedgerEntry {
    busy: bool,
    last_busy_at: Option<Instant>,
}

/// Counts for the current resume run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stats {
    resumed: u32,
    failed: u32,
    mid_turn: u32,
    total: u32,
}

/// Per-episode notification bookkeeping (reset when recovery finishes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Episode {
    notified_died: bool,
    notified_waiting: bool,
    /// The episode began with boot recovery (starts are logged as such).
    boot: bool,
    /// The episode began with the user (boot or a restart) rather than a death:
    /// an uneventful finish is not announced.
    user_initiated: bool,
    /// The run was started from Healthy by a user action on leftovers: its end is
    /// not announced.
    quiet: bool,
    live_at_death: u32,
    /// Sessions a boot owed a resume while herdr was down (0: not such a boot).
    boot_owed: u32,
    /// The first memory sample of the boot wait was already offered the
    /// whole-queue check (only that one sample may skip the sustain).
    boot_sustain_checked: bool,
    /// When the episode first waited for memory.
    await_first: Option<Instant>,
    /// How long that first wait took, set when it ended in a start.
    sustain_ms: Option<u64>,
    /// When the episode first entered `Resuming`.
    resuming_first: Option<Instant>,
}

/// The recovery policy. See the module docs.
pub struct RecoveryMachine {
    p: RecoveryParams,
    phase: Phase,
    /// `Some` iff `phase == Paused`.
    pause: Option<PauseReason>,
    server_up: bool,
    generation: Generation,
    /// Dedupe: a death of this instance was already handled.
    last_dead: Option<ServerInstance>,
    /// The instance the last start (or boot) reported; reconciled again on
    /// "Resume now" after a failed reconcile.
    last_started: Option<ServerInstance>,
    /// Deaths inside the breaker window.
    deaths: VecDeque<(Instant, DeathClass)>,
    resume_now_at: Option<Instant>,
    healthy_since: Option<Instant>,
    healthy_reset_at: Option<Instant>,
    backoff_until: Option<Instant>,
    /// The backoff the current `Down` waits out (for the reason text).
    backoff_len: Duration,
    await_started: Option<Instant>,
    cap_at: Option<Instant>,
    next_sample_at: Option<Instant>,
    stable_since: Option<Instant>,
    sample_idx: usize,
    awaiting_sample: bool,
    step_deadline: Option<Instant>,
    start_failures: u32,
    queue: Vec<QueueEntry>,
    /// Keys whose resume is running; never more than `resume_cap`.
    inflight: HashSet<SessionKey>,
    /// The last resume attempt id handed out (ids only ever grow).
    last_attempt: u64,
    /// The effective concurrency cap: `p.resume_concurrency`, dropped to 1 for
    /// the rest of an episode by a load-related resume outcome.
    resume_cap: u32,
    /// Keys whose resume succeeded since the episode began (they are usable
    /// while the rest of the queue still resumes).
    settled: HashSet<SessionKey>,
    stagger_until: Option<Instant>,
    budget: u32,
    carry_budget: Option<u32>,
    gate_bypass: bool,
    locks: HashSet<SessionKey>,
    ledger: HashMap<SessionKey, LedgerEntry>,
    /// Frozen at the first death of an episode: who was mid-turn.
    busy_at_death: HashSet<SessionKey>,
    /// Frozen with `busy_at_death`: when each session was last busy (queue order).
    last_busy_at_death: HashMap<SessionKey, Instant>,
    /// Cancels that arrived before the queue was built.
    cancelled_keys: HashSet<SessionKey>,
    stats: Stats,
    episode: Episode,
    seq: Cell<u64>,
    /// Commands produced by the current `step`.
    out: Vec<Command>,
}

impl RecoveryMachine {
    /// Starts `Healthy` with the server assumed up.
    pub fn new(params: RecoveryParams, now: Instant) -> Self {
        let _ = now;
        let resume_cap = params.resume_concurrency;
        RecoveryMachine {
            p: params,
            phase: Phase::Healthy,
            pause: None,
            server_up: true,
            generation: Generation::default(),
            last_dead: None,
            last_started: None,
            deaths: VecDeque::new(),
            resume_now_at: None,
            healthy_since: None,
            healthy_reset_at: None,
            backoff_until: None,
            backoff_len: Duration::ZERO,
            await_started: None,
            cap_at: None,
            next_sample_at: None,
            stable_since: None,
            sample_idx: 0,
            awaiting_sample: false,
            step_deadline: None,
            start_failures: 0,
            queue: Vec::new(),
            inflight: HashSet::new(),
            last_attempt: 0,
            resume_cap,
            settled: HashSet::new(),
            stagger_until: None,
            budget: 0,
            carry_budget: None,
            gate_bypass: false,
            locks: HashSet::new(),
            ledger: HashMap::new(),
            busy_at_death: HashSet::new(),
            last_busy_at_death: HashMap::new(),
            cancelled_keys: HashSet::new(),
            stats: Stats::default(),
            episode: Episode::default(),
            seq: Cell::new(0),
            out: Vec::new(),
        }
    }

    /// Feed one event; returns the commands to execute, in order. Phase changes
    /// come out as `Record`s in transition order; `EmitRecovery` comes last, once,
    /// when anything on the wire changed.
    pub fn step(&mut self, event: Event, now: Instant) -> Vec<Command> {
        let before = self.wire(0);
        self.out.clear();
        self.handle(event, now);
        let mut out = std::mem::take(&mut self.out);
        if self.wire(0) != before {
            out.push(Command::EmitRecovery);
        }
        out
    }

    /// The earliest timer relevant to the current phase.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Healthy => self.healthy_reset_at,
            Phase::Down => self.backoff_until,
            Phase::AwaitHeadroom => [self.cap_at, self.next_sample_at]
                .into_iter()
                .flatten()
                .min(),
            Phase::Restarting | Phase::Reconciling => self.step_deadline,
            Phase::Resuming => self.stagger_until,
            Phase::Paused => None,
        }
    }

    fn handle(&mut self, event: Event, now: Instant) {
        match event {
            Event::Observed(obs) => self.observe(obs, now),
            Event::Death { ev, generation } => {
                // A death only matters while a server we know of could be running.
                let relevant = match self.phase {
                    Phase::Down => false,
                    Phase::AwaitHeadroom | Phase::Paused => self.server_up,
                    _ => true,
                };
                if relevant {
                    self.process_death(&ev, generation, now);
                }
            }
            Event::HeadroomTick(s) if self.awaiting_sample => match self.phase {
                Phase::AwaitHeadroom => self.evaluate_sample(s, now),
                Phase::Resuming => self.resume_gate_sample(s, now),
                _ => {}
            },
            Event::TimerFired => self.timer(now),
            Event::ServerStarted(i) if self.phase == Phase::Restarting => {
                self.server_started(i, now)
            }
            Event::ServerStartFailed(_) if self.phase == Phase::Restarting => {
                self.start_failed(now)
            }
            Event::ReconcileDone(r) if self.phase == Phase::Reconciling => self.reconciled(r, now),
            // Accepted in every phase: `resume_result` drops a result whose
            // generation moved on, whose key is no longer in flight, or that
            // answers an earlier attempt than the one running.
            Event::ResumeResult {
                key,
                generation,
                attempt,
                outcome,
                load_related,
            } => self.resume_result(key, generation, attempt, outcome, load_related, now),
            Event::UserResumeNow => self.resume_now(now),
            Event::UserRestart { generation }
                if matches!(
                    self.phase,
                    Phase::Healthy | Phase::Down | Phase::AwaitHeadroom | Phase::Paused
                ) =>
            {
                self.process_user_restart(generation, now)
            }
            Event::BootRecovery {
                generation,
                live,
                bypass_gate,
                owed,
            } if self.phase == Phase::Healthy => {
                self.process_boot(generation, live, bypass_gate, owed, now)
            }
            Event::ExplicitStop => self.explicit_stop(),
            Event::DriverFault => self.pause(PauseReason::Internal),
            Event::UserFocus(k) => self.promote(k, false, now),
            Event::UserStartFresh(k) => self.promote(k, true, now),
            Event::Cancel(k) => self.cancel_key(&k, now),
            Event::CancelProject(project) => {
                let keys: Vec<SessionKey> = self
                    .queue
                    .iter()
                    .filter(|e| e.key.project == project)
                    .map(|e| e.key.clone())
                    .collect();
                for k in keys {
                    self.cancel_key(&k, now);
                }
            }
            Event::TransferLock(k, true) => {
                self.locks.insert(k);
            }
            Event::TransferLock(k, false) => {
                self.locks.remove(&k);
                if self.phase == Phase::Resuming {
                    self.pump(now);
                }
            }
            _ => {}
        }
    }

    fn push(&mut self, cmd: Command) {
        self.out.push(cmd);
    }

    /// Change phase, recording the transition. Leaving `Paused` drops its reason.
    fn set_phase(&mut self, to: Phase, reason: &str) {
        let from = self.phase;
        if to != Phase::Paused {
            self.pause = None;
        }
        if from == to {
            return;
        }
        self.phase = to;
        self.push(Command::Record {
            from,
            to,
            reason: reason.to_string(),
        });
    }

    /// Enter `Resuming`, remembering when the episode first did (for timing).
    fn enter_resuming(&mut self, reason: &str, now: Instant) {
        self.set_phase(Phase::Resuming, reason);
        self.episode.resuming_first.get_or_insert(now);
    }

    fn clear_timers(&mut self) {
        self.backoff_until = None;
        self.await_started = None;
        self.cap_at = None;
        self.next_sample_at = None;
        self.stable_since = None;
        self.awaiting_sample = false;
        self.step_deadline = None;
        self.stagger_until = None;
        self.healthy_reset_at = None;
    }

    fn observe(&mut self, obs: Vec<Observation>, now: Instant) {
        // Nothing reliable is seen while the server is down.
        if !self.server_up {
            return;
        }
        let live: HashSet<&SessionKey> = obs.iter().map(|o| &o.key).collect();
        self.ledger.retain(|k, _| live.contains(k));
        for o in obs {
            let entry = self.ledger.entry(o.key).or_default();
            entry.busy = o.busy;
            if o.busy {
                entry.last_busy_at = Some(now);
            }
        }
    }

    /// Freeze who was mid-turn (and when each was last busy) at the moment the
    /// sessions were interrupted; nothing is observed while the server is down.
    fn freeze_ledger(&mut self) {
        self.busy_at_death = self
            .ledger
            .iter()
            .filter(|(_, e)| e.busy)
            .map(|(k, _)| k.clone())
            .collect();
        self.last_busy_at_death = self
            .ledger
            .iter()
            .filter_map(|(k, e)| e.last_busy_at.map(|t| (k.clone(), t)))
            .collect();
        self.episode.live_at_death = self.ledger.len() as u32;
    }

    fn process_death(&mut self, ev: &DeathEvent, generation: Generation, now: Instant) {
        if self.last_dead == Some(ev.instance) {
            return;
        }
        self.last_dead = Some(ev.instance);
        self.generation = generation;
        self.server_up = false;
        if self.phase == Phase::Resuming {
            self.halve_budget();
        }
        if !self.episode.notified_died {
            self.freeze_ledger();
        }
        let class = classify(ev, &self.p);
        self.deaths.push_back((now, class));
        let window = self.p.breaker_window;
        self.deaths
            .retain(|(t, _)| now.saturating_duration_since(*t) <= window);
        // Every resume in flight is lost with the server (`halve_budget` above
        // charged all of them); the next reconcile owes them again, and their
        // late results carry the old generation and are dropped.
        self.queue.clear();
        self.inflight.clear();
        self.settled.clear();
        self.clear_timers();
        // A new death is new evidence: the memory check applies again.
        self.gate_bypass = false;

        let retrip = self
            .resume_now_at
            .is_some_and(|t| now.saturating_duration_since(t) <= self.p.resume_retrip);
        if retrip {
            self.resume_now_at = None;
            self.pause(PauseReason::ResumeRetrip(class));
            return;
        }
        if self.deaths.len() >= self.p.breaker_deaths as usize {
            self.pause(PauseReason::CrashLoop(class));
            return;
        }
        if !self.episode.notified_died {
            self.episode.notified_died = true;
            let live = self.episode.live_at_death;
            self.push(Command::Notify(NotifyKind::Died { live }));
        }
        let idx = (self.deaths.len() - 1).min(self.p.backoff.len() - 1);
        let d = self.p.backoff[idx];
        self.backoff_len = d;
        self.set_phase(Phase::Down, "herdr died");
        if d.is_zero() {
            self.enter_await_headroom(now);
        } else {
            self.backoff_until = Some(now + d);
        }
    }

    fn enter_await_headroom(&mut self, now: Instant) {
        if self.gate_bypass {
            self.proceed(now);
            return;
        }
        self.clear_timers();
        self.set_phase(Phase::AwaitHeadroom, "waiting for memory headroom");
        self.episode.await_first.get_or_insert(now);
        self.await_started = Some(now);
        self.cap_at = Some(now + self.p.headroom_cap);
        self.stable_since = None;
        self.sample_idx = 0;
        self.awaiting_sample = true;
        self.push(Command::SampleHeadroom);
    }

    fn evaluate_sample(&mut self, s: Option<HeadroomSnapshot>, now: Instant) {
        self.awaiting_sample = false;
        if self.cap_at.is_some_and(|cap| now > cap) {
            self.pause(PauseReason::HeadroomCap);
            return;
        }
        // A boot that owes sessions skips the sustain when its first sample
        // already covers every one of them at once.
        if self.episode.boot && self.episode.boot_owed > 0 && !self.episode.boot_sustain_checked {
            self.episode.boot_sustain_checked = true;
            if passes(&self.p, s.as_ref(), self.episode.boot_owed) {
                self.proceed(now);
                return;
            }
        }
        if passes(&self.p, s.as_ref(), 1) {
            let since = *self.stable_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.p.sustain {
                self.proceed(now);
                return;
            }
        } else {
            self.stable_since = None;
            if !self.episode.notified_waiting {
                self.episode.notified_waiting = true;
                self.push(Command::Notify(NotifyKind::AwaitingMemory));
            }
        }
        let gap = CHECK_GAPS_SECS[self.sample_idx.min(CHECK_GAPS_SECS.len() - 1)];
        self.next_sample_at = Some(now + Duration::from_secs(gap));
        self.sample_idx += 1;
    }

    /// Re-queue suspended entries in place (so the original order holds); only
    /// those parked by the memory gate unless `all`.
    fn requeue_suspended(&mut self, all: bool) {
        for e in &mut self.queue {
            if e.state == EntryState::Suspended && (all || e.cause == Some(SuspendCause::Gate)) {
                e.state = EntryState::Queued;
                e.cause = None;
            }
        }
    }

    /// Memory is fine: resume the rest if the server is up, else restart it.
    fn proceed(&mut self, now: Instant) {
        if let (Some(t0), None) = (self.episode.await_first, self.episode.sustain_ms) {
            self.episode.sustain_ms = Some(millis(now.saturating_duration_since(t0)));
        }
        if self.server_up {
            self.clear_timers();
            self.requeue_suspended(false);
            self.enter_resuming("memory headroom available", now);
            self.pump(now);
        } else {
            self.enter_restarting(now);
        }
    }

    fn start_how(&self) -> ServerStartHow {
        if self.episode.boot {
            ServerStartHow::BootRecovery
        } else {
            ServerStartHow::Restart
        }
    }

    fn enter_restarting(&mut self, now: Instant) {
        self.clear_timers();
        self.set_phase(Phase::Restarting, "restarting herdr");
        self.step_deadline = Some(now + START_TIMEOUT);
        let how = self.start_how();
        // Only a server still running (e.g. after an internal fault) needs stopping.
        let stop_first = self.server_up;
        self.push(Command::StartServer { stop_first, how });
        self.server_up = false;
    }

    fn enter_reconciling(&mut self, observed: ServerInstance, reason: &str, now: Instant) {
        self.clear_timers();
        self.set_phase(Phase::Reconciling, reason);
        self.step_deadline = Some(now + RECONCILE_TIMEOUT);
        self.push(Command::Reconcile {
            generation: self.generation,
            observed,
        });
    }

    fn server_started(&mut self, i: ServerInstance, now: Instant) {
        self.server_up = true;
        self.start_failures = 0;
        self.ledger.clear();
        self.last_started = Some(i);
        self.enter_reconciling(i, "herdr started", now);
    }

    /// A failed start is not a breaker death; too many in a row pause.
    fn start_failed(&mut self, now: Instant) {
        self.start_failures += 1;
        if self.start_failures >= MAX_START_FAILURES {
            self.pause(PauseReason::StartFailed);
        } else {
            self.enter_await_headroom(now);
        }
    }

    fn reconciled(&mut self, r: Result<ReconcileOutcome, String>, now: Instant) {
        match r {
            Ok(ReconcileOutcome::Adopted) => {
                self.queue.clear();
                self.finish(now);
            }
            Ok(ReconcileOutcome::NewGeneration { to_resume }) => {
                self.build_queue(to_resume);
                if self.queue.iter().any(|e| e.state == EntryState::Queued) {
                    self.clear_timers();
                    self.enter_resuming("sessions restored", now);
                    self.pump(now);
                } else {
                    self.finish(now);
                }
            }
            // Never loop on a failed reconcile.
            Err(_) => self.pause(PauseReason::ReconcileFailed),
        }
    }

    /// Order: mid-turn at death first, then most recently busy, then by name.
    fn build_queue(&mut self, to_resume: Vec<SessionKey>) {
        let cancelled = std::mem::take(&mut self.cancelled_keys);
        let mut entries: Vec<QueueEntry> = to_resume
            .into_iter()
            .filter(|k| !cancelled.contains(k))
            .map(|k| QueueEntry {
                busy_at_death: self.busy_at_death.contains(&k),
                key: k,
                generation: self.generation,
                state: EntryState::Queued,
                cause: None,
                force: false,
                fresh: false,
                cancelled: false,
                attempt: 0,
            })
            .collect();
        let recency = &self.last_busy_at_death;
        entries.sort_by(|a, b| {
            b.busy_at_death
                .cmp(&a.busy_at_death)
                .then_with(|| recency.get(&b.key).cmp(&recency.get(&a.key)))
                .then_with(|| {
                    (&a.key.project, &a.key.worktree).cmp(&(&b.key.project, &b.key.worktree))
                })
        });
        self.queue = entries;
        self.inflight.clear();
        self.stagger_until = None;
        self.stats = Stats {
            total: self.queue.len() as u32,
            ..Stats::default()
        };
        let total = self.stats.total;
        self.budget = self.carry_budget.take().unwrap_or(total);
    }

    /// A death interrupted a resume run: the next run auto-resumes at most half of
    /// what this one still had left (at least one). The next queue holds every
    /// session again, so the most important ones win the reduced budget.
    fn halve_budget(&mut self) {
        let unresumed = self.queue.iter().filter(|e| !e.cancelled).count() as u32;
        // Entries in flight already took their share of the budget.
        let inflight_share = self
            .queue
            .iter()
            .filter(|e| e.state == EntryState::Resuming && !e.force)
            .count() as u32;
        let remaining = unresumed.min(self.budget.saturating_add(inflight_share));
        let halved = remaining.div_ceil(2).max(1);
        self.carry_budget = Some(self.carry_budget.unwrap_or(u32::MAX).min(halved));
    }

    /// The next entry to resume: the first queued one no transfer holds.
    fn pick(&self) -> Option<usize> {
        self.queue
            .iter()
            .position(|e| e.state == EntryState::Queued && !self.locks.contains(&e.key))
    }

    /// Start resumes while a slot under the cap is free, the stagger has passed
    /// and (unless forced or bypassed) the memory check passes; finish when
    /// nothing is left and nothing is in flight.
    fn pump(&mut self, now: Instant) {
        self.fill(now, None);
    }

    /// [`Self::pump`], optionally with a memory sample taken in this turn
    /// (`sample = Some(s)`): each non-forced start must pass it for every session
    /// then in flight plus itself. Without a sample in hand one is requested and
    /// the fill resumes when it answers. This is the only place the memory check
    /// before a resume is made. `true`: the fill stopped because the sample
    /// failed that check.
    fn fill(&mut self, now: Instant, sample: Option<Option<HeadroomSnapshot>>) -> bool {
        while self.phase == Phase::Resuming && self.inflight.len() < self.resume_cap as usize {
            let Some(idx) = self.pick() else {
                // Entries a transfer holds wait for the unlock; work in flight
                // finishes the run from its own result.
                if self.inflight.is_empty()
                    && !self.queue.iter().any(|e| e.state == EntryState::Queued)
                {
                    self.finish(now);
                }
                return false;
            };
            // Forced entries skip the memory check and the budget but still obey
            // the cap and the stagger.
            if self.stagger_until.is_some_and(|t| now < t) {
                return false;
            }
            let force = self.queue[idx].force;
            if !force && self.budget == 0 {
                // The reduced budget is spent: park the rest, still visible and
                // resumable by the user or the healthy reset.
                for e in &mut self.queue {
                    if e.state == EntryState::Queued {
                        e.state = EntryState::Suspended;
                        e.cause = Some(SuspendCause::Budget);
                    }
                }
                if self.inflight.is_empty() {
                    self.finish(now);
                }
                return false;
            }
            if !force && !self.gate_bypass {
                let Some(s) = sample else {
                    if !self.awaiting_sample {
                        self.awaiting_sample = true;
                        self.push(Command::SampleHeadroom);
                    }
                    return false;
                };
                let sessions = self.inflight.len() as u32 + 1;
                if !passes(&self.p, s.as_ref(), sessions) {
                    // Not enough for one more alongside those running: wait for
                    // a result and check again then.
                    return true;
                }
            }
            self.start_next(idx);
        }
        false
    }

    fn start_next(&mut self, idx: usize) {
        self.last_attempt += 1;
        let e = &mut self.queue[idx];
        e.state = EntryState::Resuming;
        e.attempt = self.last_attempt;
        if !e.force {
            self.budget = self.budget.saturating_sub(1);
        }
        let cmd = Command::Resume {
            key: e.key.clone(),
            generation: e.generation,
            mode: if e.fresh {
                ResumeMode::Fresh
            } else {
                ResumeMode::Stored
            },
            attempt: e.attempt,
        };
        self.inflight.insert(e.key.clone());
        self.awaiting_sample = false;
        self.stagger_until = None;
        self.push(cmd);
    }

    /// The memory check before the next resume(s): fill the free slots from
    /// this sample. A failing check with resumes still in flight just waits for
    /// their next result (which samples again); only one that leaves nothing
    /// running parks the rest of the queue until memory recovers.
    fn resume_gate_sample(&mut self, s: Option<HeadroomSnapshot>, now: Instant) {
        self.awaiting_sample = false;
        if self.inflight.len() >= self.resume_cap as usize {
            return;
        }
        let gate_failed = self.fill(now, Some(s));
        if gate_failed && self.inflight.is_empty() {
            for e in &mut self.queue {
                if e.state == EntryState::Queued {
                    e.state = EntryState::Suspended;
                    e.cause = Some(SuspendCause::Gate);
                }
            }
            self.enter_await_headroom(now);
        }
    }

    fn resume_result(
        &mut self,
        key: SessionKey,
        generation: Generation,
        attempt: u64,
        outcome: ResumeOutcomeKind,
        load_related: bool,
        now: Instant,
    ) {
        if generation != self.generation || !self.inflight.contains(&key) {
            return;
        }
        // The running attempt's entry; a late result of an attempt a pause took
        // back (and "Resume now" started again) finds none and is dropped.
        let Some(idx) = self
            .queue
            .iter()
            .position(|e| e.key == key && e.state == EntryState::Resuming && e.attempt == attempt)
        else {
            return;
        };
        let failed = matches!(outcome, ResumeOutcomeKind::Failed(_));
        let e = self.queue.remove(idx);
        if !e.cancelled {
            match outcome {
                ResumeOutcomeKind::Failed(_) => self.stats.failed += 1,
                ResumeOutcomeKind::AlreadyRunning => {
                    self.stats.total = self.stats.total.saturating_sub(1);
                    self.settled.insert(key.clone());
                }
                ResumeOutcomeKind::Cancelled => {
                    self.stats.total = self.stats.total.saturating_sub(1);
                }
                _ => {
                    self.stats.resumed += 1;
                    if e.busy_at_death {
                        self.stats.mid_turn += 1;
                    }
                    self.settled.insert(key.clone());
                }
            }
        }
        self.inflight.remove(&key);
        // A timeout or an early exit means the machine is struggling: one at a
        // time for the rest of the episode.
        if load_related && self.resume_cap > 1 {
            self.resume_cap = 1;
        }
        // Clean resumes follow each other at once; trouble buys a pause.
        if failed || load_related {
            self.stagger_until = Some(now + self.p.stagger);
        }
        self.pump(now);
    }

    fn timer(&mut self, now: Instant) {
        let due = |t: Option<Instant>| t.is_some_and(|t| now >= t);
        match self.phase {
            Phase::Healthy => {
                if due(self.healthy_reset_at) {
                    self.healthy_reset(now);
                }
            }
            Phase::Down => {
                if due(self.backoff_until) {
                    self.backoff_until = None;
                    self.enter_await_headroom(now);
                }
            }
            Phase::AwaitHeadroom => {
                // The cap wins a tie with a sample.
                if due(self.cap_at) {
                    self.pause(PauseReason::HeadroomCap);
                } else if !self.awaiting_sample && due(self.next_sample_at) {
                    self.next_sample_at = None;
                    self.awaiting_sample = true;
                    self.push(Command::SampleHeadroom);
                }
            }
            Phase::Restarting => {
                if due(self.step_deadline) {
                    self.step_deadline = None;
                    self.start_failed(now);
                }
            }
            Phase::Reconciling => {
                if due(self.step_deadline) {
                    self.pause(PauseReason::ReconcileFailed);
                }
            }
            Phase::Resuming => {
                if due(self.stagger_until) {
                    self.stagger_until = None;
                    self.pump(now);
                }
            }
            Phase::Paused => {}
        }
    }

    /// Continuously healthy long enough: forget past deaths, start failures and the
    /// reduced budget, and resume what the budget left suspended.
    fn healthy_reset(&mut self, now: Instant) {
        self.deaths.clear();
        self.resume_now_at = None;
        self.carry_budget = None;
        self.start_failures = 0;
        self.healthy_reset_at = None;
        let mut n = 0;
        for e in &mut self.queue {
            if e.state == EntryState::Suspended && e.cause == Some(SuspendCause::Budget) {
                e.state = EntryState::Queued;
                e.cause = None;
                n += 1;
            }
        }
        if n > 0 {
            self.budget = n;
            self.stats = Stats {
                total: n,
                ..Stats::default()
            };
            self.enter_resuming("resuming suspended sessions", now);
            self.pump(now);
        }
    }

    /// Focus / start fresh: move the entry to the front, forced (no memory check,
    /// no budget). Ignored while the server is down, for unknown keys, and for the
    /// entry already in flight.
    fn promote(&mut self, k: SessionKey, fresh: bool, now: Instant) {
        if !self.server_up {
            return;
        }
        let allowed = match self.phase {
            Phase::Healthy | Phase::AwaitHeadroom | Phase::Resuming => true,
            Phase::Paused => self.pause == Some(PauseReason::HeadroomCap),
            _ => false,
        };
        if !allowed {
            return;
        }
        let Some(idx) = self.queue.iter().position(|e| e.key == k) else {
            return;
        };
        if self.queue[idx].state == EntryState::Resuming {
            return;
        }
        let mut e = self.queue.remove(idx);
        e.state = EntryState::Queued;
        e.cause = None;
        e.force = true;
        e.fresh |= fresh;
        self.queue.insert(0, e);
        if self.phase != Phase::Resuming {
            // A run the user starts on leftovers is not announced when it ends.
            if self.phase == Phase::Healthy {
                self.episode.quiet = true;
            }
            // Leaving a wait (or a pause) starts at once: no stagger, no sample.
            self.clear_timers();
            self.enter_resuming("resume requested for a session", now);
        }
        self.pump(now);
    }

    /// Kill / worktree removal: drop the entry (or discard the in-flight result).
    fn cancel_key(&mut self, k: &SessionKey, now: Instant) {
        if let Some(idx) = self.queue.iter().position(|e| &e.key == k) {
            if self.queue[idx].state == EntryState::Resuming {
                // The running resume cannot be aborted; only its outcome is dropped.
                self.queue[idx].cancelled = true;
            } else {
                self.queue.remove(idx);
            }
            self.stats.total = self.stats.total.saturating_sub(1);
        } else if self.queue.is_empty() && self.phase != Phase::Healthy {
            // The queue is not built yet: apply the cancel when it is.
            self.cancelled_keys.insert(k.clone());
        }
        // A killed session is no longer usable.
        self.settled.remove(k);
        if self.phase == Phase::Resuming {
            self.pump(now);
        }
    }

    /// End of a run (nothing in flight): wait for memory again if gate-parked
    /// entries remain, else back to Healthy with the "done" notification. The
    /// settled set survives: a later run on leftovers keeps them usable.
    fn finish(&mut self, now: Instant) {
        debug_assert!(self.inflight.is_empty(), "finish with resumes in flight");
        if self
            .queue
            .iter()
            .any(|e| e.state == EntryState::Suspended && e.cause == Some(SuspendCause::Gate))
        {
            self.enter_await_headroom(now);
            return;
        }
        self.clear_timers();
        self.inflight.clear();
        self.set_phase(Phase::Healthy, "recovery finished");
        self.healthy_since = Some(now);
        self.healthy_reset_at = Some(now + self.p.healthy_reset);
        self.gate_bypass = false;
        let suspended = self.suspended_count() as u32;
        let s = self.stats;
        let uneventful = s.resumed == 0 && s.failed == 0 && suspended == 0;
        // A user-started episode that had nothing to do is not announced (the user
        // is looking at the app), nor is a run the user started on leftovers.
        let quiet = self.episode.quiet || (self.episode.user_initiated && uneventful);
        if !quiet {
            self.push(Command::Notify(NotifyKind::Done {
                resumed: s.resumed,
                mid_turn: s.mid_turn,
                failed: s.failed,
                suspended,
            }));
        }
        if let Some(t0) = self.episode.resuming_first {
            self.push(Command::RecordTiming {
                sustain_ms: self.episode.sustain_ms.unwrap_or(0),
                resume_ms: millis(now.saturating_duration_since(t0)),
                sessions: s.total,
                failed: s.failed,
                final_cap: self.resume_cap,
            });
        }
        self.resume_cap = self.p.resume_concurrency;
        self.episode = Episode::default();
    }

    fn pause(&mut self, reason: PauseReason) {
        self.clear_timers();
        // Every resume in flight goes back to the queue; their late results no
        // longer match the in-flight set (or, once started again, the running
        // attempt) and are dropped.
        for k in std::mem::take(&mut self.inflight) {
            if let Some(e) = self.queue.iter_mut().find(|e| e.key == k) {
                e.state = EntryState::Queued;
            }
        }
        self.gate_bypass = false;
        let text = payload::paused_text(&reason, &self.p);
        self.set_phase(Phase::Paused, &text);
        self.pause = Some(reason);
        self.push(Command::Notify(NotifyKind::Paused { reason: text }));
    }

    /// "Resume now": skip the remaining waits and checks for this episode.
    fn resume_now(&mut self, now: Instant) {
        match self.phase {
            Phase::Restarting | Phase::Reconciling => {}
            Phase::Healthy => {
                // "Resume all" for sessions the budget left suspended.
                if self.suspended_count() > 0 && self.server_up {
                    self.gate_bypass = true;
                    self.resume_now_at = Some(now);
                    self.episode.quiet = true;
                    self.resume_all_suspended(now);
                }
            }
            Phase::Down => {
                self.gate_bypass = true;
                self.deaths.clear();
                self.resume_now_at = Some(now);
                self.enter_restarting(now);
            }
            Phase::AwaitHeadroom => {
                self.gate_bypass = true;
                self.resume_now_at = Some(now);
                if self.server_up {
                    self.resume_all_suspended(now);
                } else {
                    self.deaths.clear();
                    self.enter_restarting(now);
                }
            }
            Phase::Resuming => {
                self.gate_bypass = true;
                self.resume_now_at = Some(now);
                self.requeue_suspended(true);
                self.budget = u32::MAX;
                self.pump(now);
            }
            Phase::Paused => {
                self.gate_bypass = true;
                self.resume_now_at = Some(now);
                match self.pause {
                    Some(PauseReason::ReconcileFailed) => match self.last_started {
                        Some(i) if self.server_up => {
                            self.enter_reconciling(i, "resume requested", now)
                        }
                        _ => {
                            self.deaths.clear();
                            self.enter_restarting(now);
                        }
                    },
                    Some(PauseReason::HeadroomCap) if self.server_up => {
                        self.resume_all_suspended(now);
                    }
                    _ => {
                        self.deaths.clear();
                        self.enter_restarting(now);
                    }
                }
            }
        }
    }

    /// Re-queue every suspended entry with an unlimited budget and resume.
    fn resume_all_suspended(&mut self, now: Instant) {
        self.clear_timers();
        self.requeue_suspended(true);
        self.budget = u32::MAX;
        self.enter_resuming("resume requested", now);
        self.pump(now);
    }

    /// Settings → Restart: bypasses the breaker, the memory check and the re-trip
    /// rule; no "herdr stopped" notification (the user asked for it).
    fn process_user_restart(&mut self, generation: Generation, now: Instant) {
        let was_up = self.server_up;
        // Restarting a live server interrupts its sessions like a death does.
        if was_up && !self.episode.notified_died {
            self.freeze_ledger();
        }
        self.generation = generation;
        self.deaths.clear();
        self.gate_bypass = true;
        self.resume_now_at = None;
        self.queue.clear();
        self.inflight.clear();
        self.settled.clear();
        self.resume_cap = self.p.resume_concurrency;
        self.carry_budget = None;
        self.start_failures = 0;
        self.clear_timers();
        self.episode.notified_died = true;
        self.episode.user_initiated = true;
        self.episode.boot = false;
        self.episode.boot_owed = 0;
        // The restart is timed on its own.
        self.episode.await_first = None;
        self.episode.sustain_ms = None;
        self.episode.resuming_first = None;
        self.set_phase(Phase::Restarting, "restart requested");
        self.step_deadline = Some(now + START_TIMEOUT);
        self.push(Command::StartServer {
            stop_first: was_up,
            how: ServerStartHow::Restart,
        });
        self.server_up = false;
    }

    /// App launch found herdr down (`live = None`) or replaced (`Some`). Never
    /// counts toward the breaker and never announces a death.
    fn process_boot(
        &mut self,
        generation: Generation,
        live: Option<ServerInstance>,
        bypass_gate: bool,
        owed: u32,
        now: Instant,
    ) {
        self.generation = generation;
        self.settled.clear();
        self.resume_cap = self.p.resume_concurrency;
        self.episode.notified_died = true;
        self.episode.boot = true;
        self.episode.user_initiated = true;
        // Only a launch that waits for memory before starting herdr can skip
        // that wait.
        self.episode.boot_owed = if live.is_none() && !bypass_gate {
            owed
        } else {
            0
        };
        self.episode.boot_sustain_checked = false;
        match live {
            None => {
                self.server_up = false;
                self.gate_bypass = bypass_gate;
                self.backoff_len = Duration::ZERO;
                self.set_phase(Phase::Down, "herdr was not running at launch");
                self.enter_await_headroom(now);
            }
            Some(i) => {
                // Closing restored shells costs nothing: reconcile without waiting;
                // each resume is still memory-checked.
                self.server_up = true;
                self.last_dead = None;
                self.last_started = Some(i);
                self.enter_reconciling(i, "herdr was replaced while Keitora was closed", now);
            }
        }
    }

    /// Settings → Stop: drop everything and stay Healthy with no server; only a
    /// user restart starts it again.
    fn explicit_stop(&mut self) {
        self.queue.clear();
        self.inflight.clear();
        self.settled.clear();
        self.resume_cap = self.p.resume_concurrency;
        self.server_up = false;
        self.carry_budget = None;
        self.cancelled_keys.clear();
        self.clear_timers();
        self.gate_bypass = false;
        self.set_phase(Phase::Healthy, "stopped by the user");
        self.episode = Episode::default();
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The keys whose resume is in flight.
    pub fn inflight(&self) -> &HashSet<SessionKey> {
        &self.inflight
    }

    /// The keys whose resume succeeded since the episode began.
    pub fn settled(&self) -> &HashSet<SessionKey> {
        &self.settled
    }

    /// The `daemon://recovery` payload; every call takes the next `seq`.
    pub fn snapshot(&self) -> RecoveryPayload {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        self.wire(seq)
    }

    fn suspended_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|e| e.state == EntryState::Suspended)
            .count()
    }

    fn situation(&self) -> Situation<'_> {
        match self.phase {
            Phase::Healthy => Situation::Healthy {
                suspended: self.suspended_count(),
            },
            Phase::Down => Situation::Down {
                backoff: self.backoff_len,
            },
            Phase::AwaitHeadroom => Situation::AwaitHeadroom {
                server_up: self.server_up,
            },
            Phase::Restarting => Situation::Restarting,
            Phase::Reconciling => Situation::Reconciling,
            Phase::Resuming => Situation::Resuming {
                done: self.stats.resumed + self.stats.failed,
                total: self.stats.total,
            },
            Phase::Paused => match &self.pause {
                Some(reason) => Situation::Paused(reason),
                None => Situation::Paused(&PauseReason::Internal),
            },
        }
    }

    /// The payload for `seq` (pure; `snapshot` picks the seq).
    fn wire(&self, seq: u64) -> RecoveryPayload {
        let situation = self.situation();
        let queue = self
            .queue
            .iter()
            .map(|e| QueueItem {
                project: e.key.project.clone(),
                worktree: e.key.worktree.clone(),
                state: match e.state {
                    EntryState::Queued => "queued",
                    EntryState::Resuming => "resuming",
                    EntryState::Suspended => "suspended",
                },
            })
            .collect();
        let mut settled: Vec<SessionRef> = self
            .settled
            .iter()
            .map(|k| SessionRef {
                project: k.project.clone(),
                worktree: k.worktree.clone(),
            })
            .collect();
        settled.sort_by(|a, b| (&a.project, &a.worktree).cmp(&(&b.project, &b.worktree)));
        let can_resume_now = matches!(
            self.phase,
            Phase::Down | Phase::AwaitHeadroom | Phase::Paused
        ) || (self.phase == Phase::Healthy && self.suspended_count() > 0);
        RecoveryPayload {
            phase: self.phase,
            reason: payload::reason_text(situation, &self.p),
            reason_code: payload::reason_code(situation),
            queue,
            settled,
            resumed: self.stats.resumed,
            failed: self.stats.failed,
            total: self.stats.total,
            generation: self.generation.0,
            can_resume_now,
            seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::watchdog::DeathSource;
    use crate::headroom::MIB;
    use std::time::SystemTime;

    const GIB: u64 = 1024 * MIB;

    fn snap_mib(commit_mib: u64, phys_mib: u64) -> HeadroomSnapshot {
        HeadroomSnapshot {
            commit_avail: commit_mib * MIB,
            phys_avail: phys_mib * MIB,
            pressure: None,
        }
    }

    fn key(p: &str, w: &str) -> SessionKey {
        SessionKey {
            project: p.into(),
            worktree: w.into(),
        }
    }

    fn death(uptime_secs: u64, headroom: Option<HeadroomSnapshot>) -> DeathEvent {
        DeathEvent {
            exit_code: Some(-1073740791),
            signal: None,
            source: DeathSource::Child,
            uptime: Duration::from_secs(uptime_secs),
            headroom_at_death: headroom,
            instance: ServerInstance {
                pid: 42,
                start_time: 7,
            },
            detected_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn headroom_passes_requires_commit_margin_plus_session_cost() {
        let p = RecoveryParams::default();
        assert_eq!(
            headroom::required_bytes(p.base_margin, p.per_session, 1),
            4696 * MIB
        );
        let ample = 64 * GIB / MIB;
        // 4.5 GiB = 4608 MiB < 4696 MiB.
        assert!(!passes(&p, Some(&snap_mib(4608, ample)), 1));
        // 4.6 GiB ~ 4710 MiB >= 4696 MiB.
        assert!(passes(&p, Some(&snap_mib(4710, ample)), 1));
        assert!(passes(&p, Some(&snap_mib(4696, ample)), 1));
        assert!(!passes(&p, Some(&snap_mib(4695, ample)), 1));
    }

    #[test]
    fn headroom_fails_when_physical_below_margin_plus_session_cost() {
        let p = RecoveryParams::default();
        assert_eq!(headroom::required_phys_bytes(p.per_session, 1), 1624 * MIB);
        let ample = 64 * GIB / MIB;
        // 1.5 GiB = 1536 MiB < 1624 MiB.
        assert!(!passes(&p, Some(&snap_mib(ample, 1536)), 1));
        // 1.7 GiB ~ 1741 MiB >= 1624 MiB.
        assert!(passes(&p, Some(&snap_mib(ample, 1741)), 1));
        assert!(passes(&p, Some(&snap_mib(ample, 1624)), 1));
        assert!(!passes(&p, Some(&snap_mib(ample, 1623)), 1));
    }

    #[test]
    fn headroom_requirement_scales_with_concurrent_sessions() {
        let p = RecoveryParams::default();
        let ample = 64 * GIB / MIB;
        // Commit: 4096 MiB margin + 600 MiB per session.
        for (n, need) in [(1u32, 4696u64), (2, 5296), (3, 5896)] {
            assert_eq!(
                headroom::required_bytes(p.base_margin, p.per_session, n),
                need * MIB
            );
            assert!(passes(&p, Some(&snap_mib(need, ample)), n), "{n}");
            assert!(!passes(&p, Some(&snap_mib(need - 1, ample)), n), "{n}");
        }
        // Physical: 1024 MiB margin + 600 MiB per session.
        for (n, need) in [(1u32, 1624u64), (2, 2224), (3, 2824)] {
            assert_eq!(headroom::required_phys_bytes(p.per_session, n), need * MIB);
            assert!(passes(&p, Some(&snap_mib(ample, need)), n), "{n}");
            assert!(!passes(&p, Some(&snap_mib(ample, need - 1)), n), "{n}");
        }
        // Enough for one is not enough for three.
        assert!(passes(&p, Some(&snap_mib(4696, ample)), 1));
        assert!(!passes(&p, Some(&snap_mib(4696, ample)), 3));
        // An unknown snapshot passes for any count.
        assert!(passes(&p, None, 3));
    }

    #[test]
    fn macos_warn_pressure_fails_gate() {
        let p = RecoveryParams::default();
        let mut s = snap_mib(64 * 1024, 64 * 1024);
        assert!(passes(&p, Some(&s), 1));
        s.pressure = Some(MemoryPressure::Normal);
        assert!(passes(&p, Some(&s), 1));
        s.pressure = Some(MemoryPressure::Warn);
        assert!(!passes(&p, Some(&s), 1));
        s.pressure = Some(MemoryPressure::Critical);
        assert!(!passes(&p, Some(&s), 1));
    }

    #[test]
    fn headroom_unknown_snapshot_counts_as_pass() {
        assert!(passes(&RecoveryParams::default(), None, 1));
    }

    #[test]
    fn classify_memory_pressure_early_death_unknown() {
        let p = RecoveryParams::default();
        let ample = snap_mib(64 * 1024, 64 * 1024);
        let mut warn = ample;
        warn.pressure = Some(MemoryPressure::Warn);
        let rows = [
            // Commit below the base margin.
            (
                death(3600, Some(snap_mib(4095, 64 * 1024))),
                DeathClass::MemoryPressure,
            ),
            // Physical below the physical margin.
            (
                death(3600, Some(snap_mib(64 * 1024, 1023))),
                DeathClass::MemoryPressure,
            ),
            (death(3600, Some(warn)), DeathClass::MemoryPressure),
            // Memory beats early.
            (
                death(5, Some(snap_mib(100, 100))),
                DeathClass::MemoryPressure,
            ),
            (death(5, Some(ample)), DeathClass::EarlyDeath),
            (death(29, None), DeathClass::EarlyDeath),
            (death(30, None), DeathClass::Unknown),
            (death(3600, Some(ample)), DeathClass::Unknown),
            // Exactly at the margins is not pressure.
            (death(3600, Some(snap_mib(4096, 1024))), DeathClass::Unknown),
        ];
        for (ev, want) in rows {
            assert_eq!(classify(&ev, &p), want, "{ev:?}");
        }
    }

    #[test]
    fn render_notifications_exact_text() {
        let died = |live| render(&NotifyKind::Died { live });
        assert_eq!(
            died(1),
            (
                "herdr stopped".into(),
                "1 session will restart automatically.".into()
            )
        );
        assert_eq!(died(4).1, "4 sessions will restart automatically.");
        assert_eq!(died(0).1, "Restarting automatically.");

        assert_eq!(
            render(&NotifyKind::AwaitingMemory),
            (
                "Waiting for memory".into(),
                "The system is low on memory. Sessions will resume once it frees up.".into()
            )
        );

        let done = |resumed, mid_turn, failed, suspended| {
            render(&NotifyKind::Done {
                resumed,
                mid_turn,
                failed,
                suspended,
            })
        };
        assert_eq!(
            done(11, 3, 0, 0),
            (
                "Sessions resumed".into(),
                "Resumed 11 sessions; 3 were mid-turn and lost that turn.".into()
            )
        );
        assert_eq!(
            done(1, 1, 0, 0).1,
            "Resumed 1 session; 1 was mid-turn and lost that turn."
        );
        assert_eq!(done(2, 0, 0, 0).1, "Resumed 2 sessions.");
        assert_eq!(
            done(0, 0, 0, 0),
            (
                "Sessions resumed".into(),
                "herdr restarted. No sessions needed resuming.".into()
            )
        );
        assert_eq!(
            done(3, 0, 2, 0),
            (
                "Recovery finished with problems".into(),
                "Resumed 3 sessions; 2 could not be resumed.".into()
            )
        );
        assert_eq!(
            done(4, 0, 0, 8).1,
            "Resumed 4 sessions; 8 suspended \u{2014} open Keitora to resume them."
        );
        assert_eq!(
            done(4, 2, 1, 1).1,
            "Resumed 4 sessions; 2 were mid-turn and lost that turn; 1 could not be resumed; \
             1 suspended \u{2014} open Keitora to resume it."
        );

        assert_eq!(
            render(&NotifyKind::Paused {
                reason: "herdr could not be started.".into()
            }),
            (
                "Recovery paused".into(),
                "herdr could not be started. Open Keitora to resume.".into()
            )
        );
    }

    fn entry(k: SessionKey, state: EntryState, cause: Option<SuspendCause>) -> QueueEntry {
        QueueEntry {
            key: k,
            generation: Generation(3),
            state,
            cause,
            busy_at_death: false,
            force: false,
            fresh: false,
            cancelled: false,
            attempt: 0,
        }
    }

    #[test]
    fn snapshot_payload_shape() {
        let base = Instant::now();
        let mut m = RecoveryMachine::new(RecoveryParams::default(), base);
        let s1 = m.snapshot();
        assert_eq!(s1.phase, Phase::Healthy);
        assert_eq!(s1.seq, 1);
        assert!(s1.reason.is_none() && s1.reason_code.is_none());
        assert!(!s1.can_resume_now);
        assert_eq!(m.snapshot().seq, 2, "seq increments on every snapshot");

        m.generation = Generation(3);
        m.phase = Phase::Resuming;
        m.queue = vec![
            entry(key("p", "a"), EntryState::Resuming, None),
            entry(key("p", "b"), EntryState::Queued, None),
            entry(
                key("q", "c"),
                EntryState::Suspended,
                Some(SuspendCause::Gate),
            ),
        ];
        m.stats = Stats {
            resumed: 1,
            failed: 1,
            mid_turn: 0,
            total: 5,
        };
        let s = m.snapshot();
        assert_eq!(s.seq, 3);
        let states: Vec<_> = s.queue.iter().map(|i| i.state).collect();
        assert_eq!(states, vec!["resuming", "queued", "suspended"]);
        assert_eq!(s.queue[2].project, "q");
        assert_eq!(s.queue[2].worktree, "c");
        assert_eq!((s.resumed, s.failed, s.total, s.generation), (1, 1, 5, 3));
        assert_eq!(
            s.reason.as_deref(),
            Some("Resuming sessions (2 of 5)\u{2026}")
        );
        assert!(!s.can_resume_now);

        let v = serde_json::to_value(&s).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 11);
        assert_eq!(v["settled"], serde_json::json!([]));
        assert_eq!(v["phase"], "Resuming");
        assert_eq!(v["queue"][1]["state"], "queued");
        assert_eq!(v["seq"], 3);
        assert!(v["reason_code"].is_null());

        // can_resume_now truth table.
        for (phase, suspended, want) in [
            (Phase::Healthy, false, false),
            (Phase::Healthy, true, true),
            (Phase::Down, false, true),
            (Phase::AwaitHeadroom, false, true),
            (Phase::Restarting, false, false),
            (Phase::Reconciling, false, false),
            (Phase::Resuming, false, false),
            (Phase::Resuming, true, false),
            (Phase::Paused, false, true),
        ] {
            m.phase = phase;
            m.pause = (phase == Phase::Paused).then_some(PauseReason::HeadroomCap);
            m.queue = if suspended {
                vec![entry(
                    key("p", "a"),
                    EntryState::Suspended,
                    Some(SuspendCause::Budget),
                )]
            } else {
                Vec::new()
            };
            assert_eq!(m.snapshot().can_resume_now, want, "{phase:?} {suspended}");
        }

        // Healthy with budget leftovers carries the suspended reason.
        m.phase = Phase::Healthy;
        m.pause = None;
        m.queue = vec![entry(
            key("p", "a"),
            EntryState::Suspended,
            Some(SuspendCause::Budget),
        )];
        let s = m.snapshot();
        assert_eq!(s.reason_code, Some("suspended"));
        assert_eq!(
            s.reason.as_deref(),
            Some("1 session suspended. Click one to resume it, or resume them all.")
        );

        m.phase = Phase::Paused;
        m.pause = Some(PauseReason::StartFailed);
        let s = m.snapshot();
        assert_eq!(s.reason_code, Some("start_failed"));
        assert_eq!(s.reason.as_deref(), Some("herdr could not be started."));
    }

    #[test]
    fn recovery_params_from_config_converts_units() {
        let cfg = RecoveryConfig {
            headroom_base_mib: 10,
            headroom_per_session_mib: 3,
            headroom_sustain_secs: 900,
            headroom_cap_secs: 120,
            stagger_secs: 2,
            resume_concurrency: 3,
            backoff_secs: vec![],
            breaker_deaths: 0,
            breaker_window_secs: 61,
            early_death_secs: 9,
            resume_retrip_secs: 7,
            healthy_reset_secs: 8,
        };
        let p = RecoveryParams::from_config(&cfg);
        assert_eq!(p.base_margin, 10 * MIB);
        assert_eq!(p.per_session, 3 * MIB);
        // Normalized first: sustain cut to the cap, backoff [0], breaker >= 1.
        assert_eq!(p.sustain, Duration::from_secs(120));
        assert_eq!(p.headroom_cap, Duration::from_secs(120));
        assert_eq!(p.backoff, vec![Duration::ZERO]);
        assert_eq!(p.breaker_deaths, 1);
        assert_eq!(p.stagger, Duration::from_secs(2));
        assert_eq!(p.resume_concurrency, 3);
        assert_eq!(p.breaker_window, Duration::from_secs(61));
        assert_eq!(p.early_death, Duration::from_secs(9));
        assert_eq!(p.resume_retrip, Duration::from_secs(7));
        assert_eq!(p.healthy_reset, Duration::from_secs(8));
    }

    #[test]
    fn recovery_params_default_matches_documented_values() {
        let p = RecoveryParams::default();
        assert_eq!(p.base_margin, 4 * GIB);
        assert_eq!(p.per_session, 600 * MIB);
        assert_eq!(p.sustain, Duration::from_secs(60));
        assert_eq!(p.headroom_cap, Duration::from_secs(600));
        assert_eq!(p.stagger, Duration::from_secs(5));
        assert_eq!(p.resume_concurrency, 3);
        assert_eq!(
            p.backoff,
            vec![
                Duration::ZERO,
                Duration::from_secs(60),
                Duration::from_secs(300)
            ]
        );
        assert_eq!(p.breaker_deaths, 3);
        assert_eq!(p.breaker_window, Duration::from_secs(1800));
        assert_eq!(p.early_death, Duration::from_secs(30));
        assert_eq!(p.resume_retrip, Duration::from_secs(600));
        assert_eq!(p.healthy_reset, Duration::from_secs(1800));
    }

    #[test]
    fn phase_names_match_lifecycle_records() {
        for (phase, name) in [
            (Phase::Healthy, "healthy"),
            (Phase::Down, "down"),
            (Phase::AwaitHeadroom, "await_headroom"),
            (Phase::Restarting, "restarting"),
            (Phase::Reconciling, "reconciling"),
            (Phase::Resuming, "resuming"),
            (Phase::Paused, "paused"),
        ] {
            assert_eq!(phase.as_str(), name);
        }
    }

    // ---- step() harness ----

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn ample() -> HeadroomSnapshot {
        snap_mib(64 * 1024, 64 * 1024)
    }

    fn low() -> HeadroomSnapshot {
        snap_mib(1024, 64 * 1024)
    }

    fn inst(pid: u32) -> ServerInstance {
        ServerInstance {
            pid,
            start_time: u64::from(pid),
        }
    }

    fn death_of(pid: u32) -> DeathEvent {
        DeathEvent {
            instance: inst(pid),
            ..death(3600, None)
        }
    }

    fn fresh() -> (RecoveryMachine, Instant) {
        let base = Instant::now();
        (RecoveryMachine::new(RecoveryParams::default(), base), base)
    }

    fn records(cmds: &[Command]) -> Vec<(Phase, Phase)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Record { from, to, .. } => Some((*from, *to)),
                _ => None,
            })
            .collect()
    }

    fn notifies(cmds: &[Command]) -> Vec<NotifyKind> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Notify(n) => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    fn resumes(cmds: &[Command]) -> Vec<SessionKey> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Resume { key, .. } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    fn has_start(cmds: &[Command]) -> bool {
        cmds.iter()
            .any(|c| matches!(c, Command::StartServer { .. }))
    }

    fn has_sample(cmds: &[Command]) -> bool {
        cmds.contains(&Command::SampleHeadroom)
    }

    fn queue_keys(m: &RecoveryMachine) -> Vec<SessionKey> {
        m.queue.iter().map(|e| e.key.clone()).collect()
    }

    fn queue_states(m: &RecoveryMachine) -> Vec<(EntryState, Option<SuspendCause>)> {
        m.queue.iter().map(|e| (e.state, e.cause)).collect()
    }

    /// One `step`, with the invariants every step must keep checked on the way.
    fn run(m: &mut RecoveryMachine, now: Instant, ev: Event) -> Vec<Command> {
        let before = m.phase;
        let bypass_before = m.gate_bypass;
        let cmds = m.step(ev.clone(), now);
        // The server is only ever started after a passing memory check, a user
        // action, boot recovery, or a retry inside an episode the user already
        // exempted from the check.
        if has_start(&cmds) {
            let allowed = match &ev {
                // The memory wait checks one session; a boot that skips the
                // sustain checked the whole queue, which implies one.
                Event::HeadroomTick(s) => passes(&m.p, s.as_ref(), 1),
                Event::UserResumeNow | Event::UserRestart { .. } | Event::BootRecovery { .. } => {
                    true
                }
                Event::ServerStartFailed(_) | Event::TimerFired => bypass_before,
                _ => false,
            };
            assert!(allowed, "StartServer after {ev:?}: {cmds:?}");
        }
        // EmitRecovery at most once, and last.
        let emits = cmds.iter().filter(|c| **c == Command::EmitRecovery).count();
        assert!(emits <= 1, "{cmds:?}");
        if emits == 1 {
            assert_eq!(cmds.last(), Some(&Command::EmitRecovery));
        }
        // Records chain from the old phase to the new one, in order.
        let mut cur = before;
        for (from, to) in records(&cmds) {
            assert_eq!(from, cur, "{ev:?} {cmds:?}");
            assert_ne!(from, to);
            cur = to;
        }
        assert_eq!(cur, m.phase, "{ev:?} {cmds:?}");
        assert_eq!(m.pause.is_some(), m.phase == Phase::Paused);
        // The in-flight set is exactly the resuming entries, within the cap
        // (the cap can drop below what is already running; nothing starts then).
        let resuming: HashSet<SessionKey> = m
            .queue
            .iter()
            .filter(|e| e.state == EntryState::Resuming)
            .map(|e| e.key.clone())
            .collect();
        let in_flight = m
            .queue
            .iter()
            .filter(|e| e.state == EntryState::Resuming)
            .count();
        assert_eq!(in_flight, m.inflight.len(), "{:?}", m.queue);
        assert_eq!(resuming, m.inflight, "{:?}", m.queue);
        assert!(
            in_flight <= m.p.resume_concurrency as usize,
            "{:?}",
            m.queue
        );
        if !resumes(&cmds).is_empty() {
            assert!(in_flight <= m.resume_cap as usize, "{:?}", m.queue);
        }
        assert!(m.resume_cap >= 1 && m.resume_cap <= m.p.resume_concurrency);
        // Work is only ever in flight while resuming.
        if !m.inflight.is_empty() {
            assert_eq!(m.phase, Phase::Resuming, "{ev:?}");
        }
        // A settled key is never in flight.
        for k in &m.settled {
            assert!(!m.inflight.contains(k), "{k:?}");
        }
        cmds
    }

    /// A scripted world around the machine, answering commands the way the driver
    /// does (in the same turn) and firing timers in order.
    struct Sim {
        m: RecoveryMachine,
        base: Instant,
        now: Instant,
        headroom: Option<HeadroomSnapshot>,
        to_resume: Vec<SessionKey>,
        failing: HashSet<SessionKey>,
        /// Keys whose resume reports a load-related outcome (a timeout when
        /// also failing, an early-exit fallback otherwise).
        load: HashSet<SessionKey>,
        /// Keys the user already started again (their resume does nothing).
        running: HashSet<SessionKey>,
        pid: u32,
        gen: u64,
        /// Every command with its offset in whole seconds from `base`.
        log: Vec<(u64, Command)>,
        /// Answer `Resume` at once; otherwise park it in `pending`.
        auto_resume: bool,
        pending: VecDeque<(SessionKey, Generation, u64)>,
        answer_starts: bool,
        fail_starts: u32,
        reconcile_err: bool,
    }

    impl Sim {
        fn new() -> Self {
            Self::with(RecoveryParams::default())
        }

        fn with(p: RecoveryParams) -> Self {
            let base = Instant::now();
            Sim {
                m: RecoveryMachine::new(p, base),
                base,
                now: base,
                headroom: Some(ample()),
                to_resume: Vec::new(),
                failing: HashSet::new(),
                load: HashSet::new(),
                running: HashSet::new(),
                pid: 1,
                gen: 1,
                log: Vec::new(),
                auto_resume: true,
                pending: VecDeque::new(),
                answer_starts: true,
                fail_starts: 0,
                reconcile_err: false,
            }
        }

        fn secs(&self) -> u64 {
            (self.now - self.base).as_secs()
        }

        fn at(&mut self, secs: u64) {
            self.now = at(self.base, secs);
        }

        fn step(&mut self, ev: Event) -> Vec<Command> {
            let cmds = run(&mut self.m, self.now, ev);
            let s = self.secs();
            self.log.extend(cmds.iter().cloned().map(|c| (s, c)));
            cmds
        }

        fn answer(&mut self, c: &Command) -> Option<Event> {
            match c {
                Command::SampleHeadroom => Some(Event::HeadroomTick(self.headroom)),
                Command::StartServer { .. } if self.fail_starts > 0 => {
                    self.fail_starts -= 1;
                    Some(Event::ServerStartFailed("spawn failed".into()))
                }
                Command::StartServer { .. } if self.answer_starts => {
                    self.pid += 1;
                    Some(Event::ServerStarted(inst(self.pid)))
                }
                Command::Reconcile { .. } if self.reconcile_err => {
                    Some(Event::ReconcileDone(Err("reconcile failed".into())))
                }
                Command::Reconcile { .. } => {
                    Some(Event::ReconcileDone(Ok(ReconcileOutcome::NewGeneration {
                        to_resume: self.to_resume.clone(),
                    })))
                }
                Command::Resume {
                    key,
                    generation,
                    attempt,
                    ..
                } => {
                    if self.auto_resume {
                        Some(self.result_for(key.clone(), *generation, *attempt))
                    } else {
                        self.pending.push_back((key.clone(), *generation, *attempt));
                        None
                    }
                }
                _ => None,
            }
        }

        fn outcome_for(&self, key: &SessionKey) -> ResumeOutcomeKind {
            if self.failing.contains(key) {
                ResumeOutcomeKind::Failed("exited early".into())
            } else if self.running.contains(key) {
                ResumeOutcomeKind::AlreadyRunning
            } else if self.load.contains(key) {
                ResumeOutcomeKind::Continued
            } else {
                ResumeOutcomeKind::Resumed
            }
        }

        fn result_for(&self, key: SessionKey, generation: Generation, attempt: u64) -> Event {
            Event::ResumeResult {
                outcome: self.outcome_for(&key),
                load_related: self.load.contains(&key),
                key,
                generation,
                attempt,
            }
        }

        /// Feed `ev` and answer every resulting command without advancing time.
        fn feed(&mut self, ev: Event) -> Vec<Command> {
            let mut all = Vec::new();
            let mut q: VecDeque<Command> = self.step(ev).into();
            while let Some(c) = q.pop_front() {
                if let Some(ev) = self.answer(&c) {
                    q.extend(self.step(ev));
                }
                all.push(c);
            }
            all
        }

        /// Answer the oldest parked resume.
        fn finish_pending(&mut self) -> Vec<Command> {
            let (key, generation, attempt) = self.pending.pop_front().expect("a resume in flight");
            let ev = self.result_for(key, generation, attempt);
            self.feed(ev)
        }

        /// Answer the parked resume of `key`.
        fn finish_key(&mut self, key: &SessionKey) -> Vec<Command> {
            let i = self
                .pending
                .iter()
                .position(|(k, _, _)| k == key)
                .expect("that resume in flight");
            let (key, generation, attempt) = self.pending.remove(i).unwrap();
            let ev = self.result_for(key, generation, attempt);
            self.feed(ev)
        }

        /// Answer every parked resume, oldest first (including those the answers
        /// start).
        fn finish_all_pending(&mut self) -> Vec<Command> {
            let mut all = Vec::new();
            while !self.pending.is_empty() {
                all.extend(self.finish_pending());
            }
            all
        }

        /// Answer the resumes a death left parked: their old generation is
        /// ignored, so nothing follows.
        fn drop_stale(&mut self) {
            while let Some((key, generation, attempt)) = self.pending.pop_front() {
                assert_ne!(generation, self.m.generation, "{key:?} is not stale");
                let ev = self.result_for(key, generation, attempt);
                let c = self.feed(ev);
                assert!(c.is_empty(), "a stale result changed something: {c:?}");
            }
        }

        /// Fire timers until Healthy or Paused, a resume is parked, or the next
        /// deadline lies beyond `limit` seconds.
        fn settle_until(&mut self, limit: u64) -> Vec<Command> {
            let mut all = Vec::new();
            for _ in 0..10_000 {
                if matches!(self.m.phase, Phase::Healthy | Phase::Paused)
                    || !self.pending.is_empty()
                {
                    break;
                }
                let Some(d) = self.m.next_deadline() else {
                    break;
                };
                if d.saturating_duration_since(self.base).as_secs() > limit {
                    break;
                }
                if d > self.now {
                    self.now = d;
                }
                all.extend(self.feed(Event::TimerFired));
            }
            all
        }

        fn settle(&mut self) -> Vec<Command> {
            self.settle_until(u64::MAX)
        }

        /// The current server dies.
        fn kill(&mut self) -> Vec<Command> {
            self.gen += 1;
            let ev = Event::Death {
                ev: death_of(self.pid),
                generation: Generation(self.gen),
            };
            self.feed(ev)
        }

        fn boot_live(&mut self) -> Vec<Command> {
            self.gen += 1;
            self.feed(Event::BootRecovery {
                generation: Generation(self.gen),
                live: Some(inst(self.pid)),
                bypass_gate: false,
                owed: 0,
            })
        }

        /// Launch with herdr down, owing `owed` sessions.
        fn boot_down(&mut self, owed: u32) -> Vec<Command> {
            self.gen += 1;
            self.feed(Event::BootRecovery {
                generation: Generation(self.gen),
                live: None,
                bypass_gate: false,
                owed,
            })
        }

        fn starts_at(&self) -> Vec<u64> {
            self.log
                .iter()
                .filter(|(_, c)| matches!(c, Command::StartServer { .. }))
                .map(|(s, _)| *s)
                .collect()
        }
    }

    fn keys(names: &[&str]) -> Vec<SessionKey> {
        names.iter().map(|n| key("p", n)).collect()
    }

    /// Defaults with one resume at a time.
    fn serial() -> RecoveryParams {
        RecoveryParams {
            resume_concurrency: 1,
            ..RecoveryParams::default()
        }
    }

    fn set(names: &[&str]) -> HashSet<SessionKey> {
        keys(names).into_iter().collect()
    }

    fn timings(cmds: &[Command]) -> Vec<Command> {
        cmds.iter()
            .filter(|c| matches!(c, Command::RecordTiming { .. }))
            .cloned()
            .collect()
    }

    /// Timer then sample at `off` seconds.
    fn sample_at(
        m: &mut RecoveryMachine,
        base: Instant,
        off: u64,
        s: HeadroomSnapshot,
    ) -> Vec<Command> {
        let tc = run(m, at(base, off), Event::TimerFired);
        assert!(has_sample(&tc), "sample due at {off}: {tc:?}");
        run(m, at(base, off), Event::HeadroomTick(Some(s)))
    }

    /// From a fresh `AwaitHeadroom` entered at `t0`, feed passing samples until the
    /// server is started; returns when that happened.
    fn pass_gate(m: &mut RecoveryMachine, base: Instant, t0: u64) -> u64 {
        let mut c = run(m, at(base, t0), Event::HeadroomTick(Some(ample())));
        let mut t = t0;
        while !has_start(&c) {
            assert_eq!(m.phase, Phase::AwaitHeadroom);
            t = m.next_deadline().unwrap().duration_since(base).as_secs();
            c = sample_at(m, base, t, ample());
        }
        t
    }

    // -- gate / sustain / cap

    #[test]
    fn sustain_met_exactly_at_sixty_seconds_with_default_gaps() {
        let (mut m, base) = fresh();
        let c = run(
            &mut m,
            base,
            Event::Death {
                ev: death_of(1),
                generation: Generation(2),
            },
        );
        assert!(has_sample(&c));
        let mut samples = 1;
        let mut c = run(&mut m, base, Event::HeadroomTick(Some(ample())));
        for off in [5, 10, 20, 30, 45, 60] {
            assert!(!has_start(&c), "started early before {off}");
            assert_eq!(m.next_deadline(), Some(at(base, off)));
            c = sample_at(&mut m, base, off, ample());
            samples += 1;
        }
        assert!(has_start(&c));
        assert_eq!(samples, 7);
        assert_eq!(m.phase, Phase::Restarting);
        assert!(c.contains(&Command::StartServer {
            stop_first: false,
            how: ServerStartHow::Restart
        }));
    }

    #[test]
    fn failing_sample_resets_sustain_clock() {
        let (mut m, base) = fresh();
        run(
            &mut m,
            base,
            Event::Death {
                ev: death_of(1),
                generation: Generation(2),
            },
        );
        let c = run(&mut m, base, Event::HeadroomTick(Some(ample())));
        assert!(!has_start(&c));
        assert!(!has_start(&sample_at(&mut m, base, 5, ample())));
        assert!(!has_start(&sample_at(&mut m, base, 10, low())));
        assert_eq!(m.stable_since, None);
        for off in [20, 30, 45, 60] {
            // Would have passed at 60 without the failure at 10.
            assert!(!has_start(&sample_at(&mut m, base, off, ample())), "{off}");
        }
        assert_eq!(m.next_deadline(), Some(at(base, 90)));
        assert!(has_start(&sample_at(&mut m, base, 90, ample())));
    }

    #[test]
    fn check_gaps_grow_then_repeat_thirty() {
        let (mut m, base) = fresh();
        run(
            &mut m,
            base,
            Event::Death {
                ev: death_of(1),
                generation: Generation(2),
            },
        );
        run(&mut m, base, Event::HeadroomTick(Some(low())));
        let mut t = 0;
        let mut gaps = Vec::new();
        for _ in 0..8 {
            let next = m.next_deadline().unwrap().duration_since(base).as_secs();
            gaps.push(next - t);
            t = next;
            sample_at(&mut m, base, t, low());
        }
        assert_eq!(gaps, vec![5, 5, 10, 10, 15, 15, 30, 30]);
    }

    #[test]
    fn await_cap_pauses_with_headroom_cap_reason() {
        let (mut m, base) = fresh();
        run(
            &mut m,
            base,
            Event::Death {
                ev: death_of(1),
                generation: Generation(2),
            },
        );
        run(&mut m, base, Event::HeadroomTick(Some(low())));
        let last = loop {
            let d = m.next_deadline().unwrap();
            let t = d.duration_since(base).as_secs();
            let tc = run(&mut m, d, Event::TimerFired);
            if m.phase == Phase::Paused {
                // The cap and a sample fell due together: the cap wins.
                assert!(!has_sample(&tc));
                assert!(notifies(&tc).contains(&NotifyKind::Paused {
                    reason: "Not enough free memory after 10 minutes. Resume now ignores the \
                             memory check."
                        .into()
                }));
                break t;
            }
            assert!(has_sample(&tc));
            run(&mut m, d, Event::HeadroomTick(Some(low())));
        };
        assert_eq!(last, 600);
        assert_eq!(m.pause, Some(PauseReason::HeadroomCap));
        assert_eq!(m.snapshot().reason_code, Some("headroom_cap"));
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn awaiting_memory_notification_sent_once_per_episode() {
        let mut s = Sim::new();
        s.headroom = Some(low());
        let mut c = s.kill();
        c.extend(s.settle_until(120));
        let waiting = |c: &[Command]| {
            notifies(c)
                .iter()
                .filter(|n| **n == NotifyKind::AwaitingMemory)
                .count()
        };
        assert_eq!(waiting(&c), 1);
        assert!(
            s.log
                .iter()
                .filter(|(_, c)| *c == Command::SampleHeadroom)
                .count()
                > 3
        );
        s.headroom = Some(ample());
        let c = s.settle();
        assert_eq!(waiting(&c), 0);
        assert_eq!(s.m.phase, Phase::Healthy);

        // A new episode notifies again.
        s.headroom = Some(low());
        s.at(s.secs() + 10);
        let mut c = s.kill();
        c.extend(s.settle_until(s.secs() + 200));
        assert_eq!(waiting(&c), 1);
    }

    // -- backoff / breaker / classification

    #[test]
    fn first_death_restarts_without_backoff() {
        let (mut m, base) = fresh();
        run(
            &mut m,
            base,
            Event::Observed(vec![
                Observation {
                    key: key("p", "a"),
                    busy: true,
                },
                Observation {
                    key: key("p", "b"),
                    busy: false,
                },
            ]),
        );
        let c = run(
            &mut m,
            base,
            Event::Death {
                ev: death_of(1),
                generation: Generation(2),
            },
        );
        assert_eq!(
            records(&c),
            vec![
                (Phase::Healthy, Phase::Down),
                (Phase::Down, Phase::AwaitHeadroom)
            ]
        );
        assert_eq!(notifies(&c), vec![NotifyKind::Died { live: 2 }]);
        assert!(has_sample(&c));
        assert_eq!(c.last(), Some(&Command::EmitRecovery));
        assert_eq!(m.generation, Generation(2));
        assert!(!m.server_up);
    }

    #[test]
    fn second_death_in_window_waits_sixty_seconds() {
        let mut s = Sim::new();
        s.kill();
        s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        s.at(120);
        let c = s.kill();
        assert_eq!(s.m.phase, Phase::Down);
        assert!(!has_sample(&c));
        assert_eq!(notifies(&c), vec![NotifyKind::Died { live: 0 }]);
        assert_eq!(s.m.next_deadline(), Some(at(s.base, 180)));
        assert_eq!(
            s.m.snapshot().reason.as_deref(),
            Some("herdr stopped. Restarting in about 1 minute.")
        );
        s.at(179);
        s.feed(Event::TimerFired);
        assert_eq!(s.m.phase, Phase::Down);
        s.at(180);
        let c = s.feed(Event::TimerFired);
        assert!(records(&c).contains(&(Phase::Down, Phase::AwaitHeadroom)));
        assert!(has_sample(&c));
    }

    #[test]
    fn third_death_in_thirty_minutes_pauses_crash_loop() {
        let mut s = Sim::new();
        s.kill();
        s.settle();
        s.at(120);
        s.kill();
        s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        s.at(400);
        let c = s.kill();
        assert_eq!(s.m.phase, Phase::Paused);
        assert_eq!(s.m.pause, Some(PauseReason::CrashLoop(DeathClass::Unknown)));
        assert!(!has_start(&c));
        assert_eq!(
            notifies(&c),
            vec![NotifyKind::Paused {
                reason: "herdr stopped unexpectedly 3 times in 30 minutes.".into()
            }]
        );
        assert_eq!(s.m.snapshot().reason_code, Some("crash_loop_unknown"));
    }

    #[test]
    fn deaths_outside_window_are_pruned() {
        let mut s = Sim::new();
        s.kill();
        s.settle();
        s.at(120);
        s.kill();
        s.settle();
        s.at(31 * 60);
        s.kill();
        assert_eq!(
            s.m.phase,
            Phase::Down,
            "the first death fell out of the window"
        );
        assert_eq!(s.m.deaths.len(), 2);
    }

    #[test]
    fn healthy_for_thirty_minutes_clears_breaker() {
        let mut s = Sim::new();
        s.kill();
        s.settle();
        s.at(120);
        s.kill();
        s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.deaths.len(), 2);
        s.m.carry_budget = Some(3);
        s.m.start_failures = 2;
        let reset = s.m.next_deadline().unwrap();
        assert_eq!(reset, s.now + Duration::from_secs(1800));
        s.now = reset;
        s.feed(Event::TimerFired);
        assert!(s.m.deaths.is_empty());
        assert_eq!(s.m.carry_budget, None);
        assert_eq!(s.m.start_failures, 0);
        assert_eq!(s.m.next_deadline(), None);
        // The next death is a first death again: no backoff.
        s.kill();
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
    }

    #[test]
    fn duplicate_death_for_same_instance_is_ignored() {
        let (mut m, base) = fresh();
        let ev = Event::Death {
            ev: death_of(42),
            generation: Generation(2),
        };
        assert!(!run(&mut m, base, ev.clone()).is_empty());
        assert!(run(&mut m, at(base, 1), ev).is_empty());
        assert_eq!(m.deaths.len(), 1);
    }

    #[test]
    fn start_failure_is_not_a_breaker_death_and_three_pause() {
        let mut s = Sim::new();
        s.fail_starts = 3;
        s.kill();
        let c = s.settle();
        assert_eq!(s.m.pause, Some(PauseReason::StartFailed));
        assert_eq!(s.m.deaths.len(), 1);
        // Each retry waits for memory again (a fresh 60 s sustain).
        assert_eq!(s.starts_at(), vec![60, 120, 180]);
        assert!(notifies(&c).contains(&NotifyKind::Paused {
            reason: "herdr could not be started.".into()
        }));
    }

    #[test]
    fn reconcile_error_pauses_instead_of_looping() {
        let mut s = Sim::new();
        s.reconcile_err = true;
        s.kill();
        s.settle();
        assert_eq!(s.m.pause, Some(PauseReason::ReconcileFailed));
        let reconciles = s
            .log
            .iter()
            .filter(|(_, c)| matches!(c, Command::Reconcile { .. }))
            .count();
        assert_eq!(reconciles, 1);
        assert_eq!(s.starts_at().len(), 1);
        assert_eq!(s.m.next_deadline(), None);
    }

    #[test]
    fn restart_step_timeout_counts_as_start_failure() {
        let (mut m, base) = fresh();
        run(
            &mut m,
            base,
            Event::Death {
                ev: death_of(1),
                generation: Generation(2),
            },
        );
        let t = pass_gate(&mut m, base, 0);
        assert_eq!(m.next_deadline(), Some(at(base, t + 30)));
        run(&mut m, at(base, t + 29), Event::TimerFired);
        assert_eq!(m.phase, Phase::Restarting);
        let c = run(&mut m, at(base, t + 30), Event::TimerFired);
        assert_eq!(m.start_failures, 1);
        assert_eq!(m.phase, Phase::AwaitHeadroom);
        assert!(has_sample(&c));
        assert_eq!(m.deaths.len(), 1);
        // A start that answers after its deadline is ignored.
        assert!(run(&mut m, at(base, t + 31), Event::ServerStarted(inst(2))).is_empty());
    }

    #[test]
    fn reconcile_step_timeout_pauses() {
        let (mut m, base) = fresh();
        run(
            &mut m,
            base,
            Event::Death {
                ev: death_of(1),
                generation: Generation(2),
            },
        );
        let t = pass_gate(&mut m, base, 0);
        let c = run(&mut m, at(base, t), Event::ServerStarted(inst(2)));
        assert!(c.contains(&Command::Reconcile {
            generation: Generation(2),
            observed: inst(2)
        }));
        assert_eq!(m.next_deadline(), Some(at(base, t + 60)));
        run(&mut m, at(base, t + 60), Event::TimerFired);
        assert_eq!(m.pause, Some(PauseReason::ReconcileFailed));
    }

    // -- resume now / retrip

    #[test]
    fn resume_now_from_paused_restarts_and_clears_deaths() {
        let mut s = Sim::new();
        s.kill();
        s.settle();
        s.at(120);
        s.kill();
        s.settle();
        s.at(400);
        s.kill();
        assert_eq!(s.m.phase, Phase::Paused);
        s.to_resume = keys(&["a"]);
        s.at(410);
        let c = s.feed(Event::UserResumeNow);
        assert!(has_start(&c));
        assert!(s.m.deaths.is_empty());
        // The memory check is bypassed for the rest of the episode.
        assert!(!has_sample(&c));
        assert_eq!(resumes(&c), keys(&["a"]));
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.resume_now_at, Some(at(s.base, 410)));
    }

    #[test]
    fn death_within_ten_minutes_of_resume_now_pauses_immediately() {
        let mut s = Sim::new();
        s.headroom = Some(low());
        s.kill();
        s.settle_until(10);
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
        s.at(10);
        s.feed(Event::UserResumeNow);
        assert_eq!(s.m.phase, Phase::Healthy);
        s.at(310);
        let c = s.kill();
        assert_eq!(
            s.m.pause,
            Some(PauseReason::ResumeRetrip(DeathClass::Unknown))
        );
        assert_eq!(s.m.deaths.len(), 1, "only one death in the window");
        assert!(!has_start(&c));
        assert_eq!(s.m.snapshot().reason_code, Some("resume_retrip"));
    }

    #[test]
    fn death_after_retrip_window_is_normal() {
        let mut s = Sim::new();
        s.headroom = Some(low());
        s.kill();
        s.settle_until(10);
        s.at(10);
        s.feed(Event::UserResumeNow);
        s.at(10 + 601);
        s.kill();
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
        assert_eq!(s.m.pause, None);
    }

    #[test]
    fn user_restart_bypasses_breaker_and_headroom_and_retrip() {
        let mut s = Sim::new();
        s.kill();
        s.settle();
        s.at(120);
        s.kill();
        s.settle();
        assert_eq!(s.m.deaths.len(), 2);
        assert!(s.m.server_up);
        s.m.resume_now_at = Some(s.now);
        s.headroom = Some(low());
        s.at(400);
        let c = s.feed(Event::UserRestart {
            generation: Generation(9),
        });
        assert_eq!(
            c.iter().find(|c| matches!(c, Command::StartServer { .. })),
            Some(&Command::StartServer {
                stop_first: true,
                how: ServerStartHow::Restart
            })
        );
        assert!(!has_sample(&c));
        assert!(notifies(&c).is_empty(), "nothing to announce: {c:?}");
        assert!(s.m.deaths.is_empty());
        assert_eq!(s.m.resume_now_at, None);
        assert_eq!(s.m.phase, Phase::Healthy);
        // Two earlier deaths and a death soon after: neither breaker nor re-trip.
        s.at(460);
        let c = s.kill();
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
        assert!(notifies(&c).contains(&NotifyKind::Died { live: 0 }));
    }

    // -- resuming / stagger / ordering

    #[test]
    fn queue_order_busy_first_then_most_recent_then_name() {
        let mut s = Sim::new();
        let obs = |busy: &[&str]| {
            Event::Observed(
                [("p", "a"), ("p", "b"), ("p", "c"), ("p", "d"), ("o", "z")]
                    .iter()
                    .map(|(p, w)| Observation {
                        key: key(p, w),
                        busy: busy.contains(w),
                    })
                    .collect(),
            )
        };
        s.feed(obs(&["c", "d"]));
        s.at(10);
        s.feed(obs(&["d"]));
        s.at(20);
        s.auto_resume = false;
        s.to_resume = vec![
            key("o", "z"),
            key("p", "a"),
            key("p", "b"),
            key("p", "c"),
            key("p", "d"),
        ];
        s.kill();
        s.settle();
        // d was busy at death; c was busy most recently; then by name.
        assert_eq!(
            queue_keys(&s.m),
            vec![
                key("p", "d"),
                key("p", "c"),
                key("o", "z"),
                key("p", "a"),
                key("p", "b")
            ]
        );
        assert_eq!(
            s.pending.front().map(|(k, _, _)| k.clone()),
            Some(key("p", "d"))
        );
        let entry_d = &s.m.queue[0];
        assert!(entry_d.busy_at_death);
        assert!(!s.m.queue[1].busy_at_death);
    }

    #[test]
    fn serial_cap_resumes_one_at_a_time() {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c"]);
        let c = s.boot_live();
        assert_eq!(resumes(&c), keys(&["a"]));
        // Nothing else starts while `a` is in flight.
        for ev in [
            Event::TimerFired,
            Event::HeadroomTick(Some(ample())),
            Event::Observed(vec![]),
        ] {
            s.at(s.secs() + 20);
            assert!(resumes(&s.feed(ev)).is_empty());
        }
        assert_eq!(s.m.next_deadline(), None, "no timer while a resume runs");
        // A clean resume hands its slot on at once.
        assert_eq!(resumes(&s.finish_pending()), keys(&["b"]));
        assert_eq!(resumes(&s.finish_pending()), keys(&["c"]));
        s.finish_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
    }

    #[test]
    fn concurrency_cap_respected_and_queue_order_kept() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d", "e"]);
        // One passing sample fills every slot, in queue order.
        let c = s.boot_live();
        assert_eq!(resumes(&c), keys(&["a", "b", "c"]));
        assert_eq!(
            c.iter().filter(|c| **c == Command::SampleHeadroom).count(),
            1
        );
        assert_eq!(s.m.inflight(), &set(&["a", "b", "c"]));
        // Full: nothing more starts.
        s.at(10);
        assert!(resumes(&s.feed(Event::TimerFired)).is_empty());
        // A result frees one slot: the next in order takes it.
        assert_eq!(resumes(&s.finish_key(&key("p", "b"))), keys(&["d"]));
        assert_eq!(s.m.inflight(), &set(&["a", "c", "d"]));
        assert_eq!(resumes(&s.finish_pending()), keys(&["e"]));
        // Nothing queued: the run waits for the work in flight.
        s.finish_pending();
        s.finish_pending();
        assert_eq!(s.m.phase, Phase::Resuming);
        let c = s.finish_pending();
        assert_eq!(records(&c), vec![(Phase::Resuming, Phase::Healthy)]);
        let starts: Vec<SessionKey> = s
            .log
            .iter()
            .filter_map(|(_, c)| match c {
                Command::Resume { key, .. } => Some(key.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(starts, keys(&["a", "b", "c", "d", "e"]));
    }

    #[test]
    fn each_extra_concurrent_resume_needs_headroom_for_all_in_flight() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d"]);
        // Enough commit for two sessions, not three.
        s.headroom = Some(snap_mib(5296, 64 * 1024));
        let c = s.boot_live();
        assert_eq!(resumes(&c), keys(&["a", "b"]));
        // `c` is not parked: it waits for a result and a new check.
        assert_eq!(
            queue_states(&s.m)[2..],
            [(EntryState::Queued, None), (EntryState::Queued, None)]
        );
        assert_eq!(s.m.phase, Phase::Resuming);
        let c = s.finish_pending();
        assert!(has_sample(&c));
        assert_eq!(resumes(&c), keys(&["c"]));
    }

    #[test]
    fn no_stagger_after_a_clean_resume() {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d"]);
        s.running.insert(key("p", "b"));
        s.boot_live();
        s.at(30);
        // Resumed, AlreadyRunning: the next starts in the same turn.
        let c = s.finish_pending();
        assert_eq!(resumes(&c), keys(&["b"]));
        assert_eq!(s.m.stagger_until, None);
        let c = s.finish_pending();
        assert_eq!(resumes(&c), keys(&["c"]));
        assert_eq!(s.m.stagger_until, None);
        assert_eq!(s.m.next_deadline(), None);
    }

    #[test]
    fn stagger_follows_every_failure_and_load_related_outcome_only() {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d"]);
        // `a` fails (not load-related), `b` falls back after an early exit.
        s.failing.insert(key("p", "a"));
        s.load.insert(key("p", "b"));
        s.boot_live();
        s.at(30);
        let c = s.finish_pending();
        assert!(resumes(&c).is_empty() && !has_sample(&c));
        assert_eq!(s.m.next_deadline(), Some(at(s.base, 35)));
        s.at(34);
        assert!(s.feed(Event::TimerFired).is_empty());
        s.at(35);
        let c = s.feed(Event::TimerFired);
        assert!(has_sample(&c));
        assert_eq!(resumes(&c), keys(&["b"]));
        // A successful but load-related resume staggers too.
        s.at(40);
        let c = s.finish_pending();
        assert!(resumes(&c).is_empty());
        assert_eq!(s.m.next_deadline(), Some(at(s.base, 45)));
        assert_eq!(resumes(&s.settle()), keys(&["c"]));
        // A clean one does not.
        assert_eq!(resumes(&s.finish_pending()), keys(&["d"]));
    }

    #[test]
    fn stagger_holds_every_slot_and_forced_entries() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d", "e"]);
        s.failing.insert(key("p", "a"));
        s.boot_live();
        s.at(10);
        // `a` fails: its slot stays empty for the stagger, even for a focus.
        assert!(resumes(&s.finish_key(&key("p", "a"))).is_empty());
        assert!(resumes(&s.feed(Event::UserFocus(key("p", "e")))).is_empty());
        assert_eq!(s.m.queue[0].key, key("p", "e"));
        // A clean result inside the stagger does not start anything either.
        assert!(resumes(&s.finish_key(&key("p", "b"))).is_empty());
        assert_eq!(s.m.next_deadline(), Some(at(s.base, 15)));
        s.at(15);
        let c = s.feed(Event::TimerFired);
        // The forced entry first (no sample needed), then one checked entry.
        assert_eq!(resumes(&c), keys(&["e", "d"]));
    }

    #[test]
    fn first_load_related_outcome_drops_the_cap_to_one() {
        // A timeout: failed and load-related.
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d", "e", "f"]);
        s.failing.insert(key("p", "a"));
        s.load.insert(key("p", "a"));
        s.boot_live();
        assert_eq!(s.m.resume_cap, 3);
        s.at(10);
        s.finish_key(&key("p", "a"));
        assert_eq!(s.m.resume_cap, 1);
        // Two still in flight: nothing more starts until they drain, even
        // after the stagger.
        s.at(20);
        assert!(resumes(&s.feed(Event::TimerFired)).is_empty());
        assert!(resumes(&s.finish_key(&key("p", "b"))).is_empty());
        let c = s.finish_key(&key("p", "c"));
        assert_eq!(resumes(&c), keys(&["d"]));
        assert_eq!(s.m.inflight().len(), 1);
        // Serial for the rest of the run, then the cap is restored.
        assert_eq!(resumes(&s.finish_pending()), keys(&["e"]));
        assert_eq!(s.m.resume_cap, 1);
        s.finish_all_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.resume_cap, 3);

        // An early-exit fallback succeeds but counts as load-related.
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d"]);
        s.load.insert(key("p", "b"));
        s.boot_live();
        s.finish_key(&key("p", "b"));
        assert_eq!(s.m.resume_cap, 1);
        assert!(s.m.stagger_until.is_some());
        assert_eq!(s.m.settled(), &set(&["b"]), "the session itself came back");

        // A failure that is not load-related keeps the cap.
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d"]);
        s.failing.insert(key("p", "a"));
        s.boot_live();
        s.finish_key(&key("p", "a"));
        assert_eq!(s.m.resume_cap, 3);
        assert_eq!(s.m.stats.failed, 1);
    }

    #[test]
    fn gate_checked_before_every_resume() {
        let mut s = Sim::with(serial());
        s.to_resume = keys(&["a", "b", "c"]);
        s.boot_live();
        s.settle();
        let mut sampled = false;
        let mut n = 0;
        for (_, c) in &s.log {
            match c {
                Command::SampleHeadroom => sampled = true,
                Command::Resume { .. } => {
                    assert!(sampled, "resume without a memory check: {:?}", s.log);
                    sampled = false;
                    n += 1;
                }
                _ => {}
            }
        }
        assert_eq!(n, 3);
    }

    /// `a` resumed, then the memory check fails before `b` (one at a time).
    fn gate_fails_after_first() -> Sim {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c"]);
        s.boot_live();
        s.headroom = Some(low());
        s.at(10);
        s.finish_pending();
        s.settle_until(16);
        s
    }

    #[test]
    fn gate_failure_mid_queue_suspends_rest_and_awaits_headroom() {
        let s = gate_fails_after_first();
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
        assert!(s.m.server_up);
        assert_eq!(queue_keys(&s.m), keys(&["b", "c"]));
        assert_eq!(
            queue_states(&s.m),
            vec![(EntryState::Suspended, Some(SuspendCause::Gate)); 2]
        );
        let snap = s.m.snapshot();
        assert_eq!(
            snap.reason.as_deref(),
            Some("Waiting for free memory before resuming the remaining sessions.")
        );
        assert_eq!(
            snap.queue.iter().map(|i| i.state).collect::<Vec<_>>(),
            vec!["suspended", "suspended"]
        );
        assert!(snap.can_resume_now);
    }

    #[test]
    fn gate_passing_again_resumes_suspended_in_original_order() {
        let mut s = gate_fails_after_first();
        s.headroom = Some(ample());
        let c = s.settle();
        assert_eq!(resumes(&c), keys(&["b"]));
        assert!(records(&c).contains(&(Phase::AwaitHeadroom, Phase::Resuming)));
        assert_eq!(resumes(&s.finish_pending()), keys(&["c"]));
        s.finish_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.stats.resumed, 3);
    }

    #[test]
    fn suspended_cap_pauses_with_entries_kept() {
        let mut s = gate_fails_after_first();
        s.settle();
        assert_eq!(s.m.pause, Some(PauseReason::HeadroomCap));
        assert_eq!(queue_keys(&s.m), keys(&["b", "c"]));
        let snap = s.m.snapshot();
        assert_eq!(
            snap.queue.iter().map(|i| i.state).collect::<Vec<_>>(),
            vec!["suspended", "suspended"]
        );
        assert_eq!(snap.reason_code, Some("headroom_cap"));
    }

    #[test]
    fn done_notification_counts_resumed_midturn_failed_suspended() {
        let names: Vec<String> = (0..11).map(|i| format!("w{i:02}")).collect();
        let all: Vec<SessionKey> = names.iter().map(|n| key("p", n)).collect();
        let mut s = Sim::new();
        s.feed(Event::Observed(
            all.iter()
                .enumerate()
                .map(|(i, k)| Observation {
                    key: k.clone(),
                    busy: i < 3,
                })
                .collect(),
        ));
        s.to_resume = all.clone();
        let mut c = s.kill();
        assert_eq!(notifies(&c), vec![NotifyKind::Died { live: 11 }]);
        c.extend(s.settle());
        let done = NotifyKind::Done {
            resumed: 11,
            mid_turn: 3,
            failed: 0,
            suspended: 0,
        };
        assert_eq!(notifies(&c).last(), Some(&done));
        assert_eq!(
            render(&done).1,
            "Resumed 11 sessions; 3 were mid-turn and lost that turn."
        );

        // A failed resume is counted, not retried.
        s.failing.insert(all[5].clone());
        s.at(s.secs() + 10);
        let mut c = s.kill();
        c.extend(s.settle());
        assert_eq!(
            notifies(&c).last(),
            Some(&NotifyKind::Done {
                resumed: 10,
                mid_turn: 0,
                failed: 1,
                suspended: 0
            })
        );
    }

    #[test]
    fn a_session_the_user_already_restarted_is_not_counted() {
        let mut s = Sim::new();
        s.to_resume = keys(&["a", "b", "c"]);
        s.running.insert(key("p", "b"));
        let mut c = s.kill();
        c.extend(s.settle());
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(
            notifies(&c).last(),
            Some(&NotifyKind::Done {
                resumed: 2,
                mid_turn: 0,
                failed: 0,
                suspended: 0
            })
        );
        assert_eq!(s.m.snapshot().total, 2);
    }

    #[test]
    fn stale_generation_resume_result_ignored() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d"]);
        s.boot_live();
        let (k, g, a) = s.pending.front().cloned().unwrap();
        assert!(s
            .step(Event::ResumeResult {
                key: k.clone(),
                generation: Generation(g.0 + 5),
                attempt: a,
                outcome: ResumeOutcomeKind::Resumed,
                load_related: true,
            })
            .is_empty());
        // A result for a key that is not in flight is ignored too.
        assert!(s
            .step(Event::ResumeResult {
                key: key("p", "d"),
                generation: g,
                attempt: a,
                outcome: ResumeOutcomeKind::Failed("x".into()),
                load_related: true,
            })
            .is_empty());
        assert!(s.m.inflight.contains(&k));
        assert_eq!(s.m.inflight, set(&["a", "b", "c"]));
        assert_eq!(
            s.m.stats,
            Stats {
                total: 4,
                ..Stats::default()
            }
        );
        assert_eq!(s.m.resume_cap, 3, "an ignored result changes nothing");
        assert_eq!(s.m.stagger_until, None);
        assert!(s.m.settled.is_empty());
    }

    #[test]
    fn resume_result_accepted_in_any_phase_but_dropped_when_not_in_flight() {
        // After a pause the in-flight set is empty: a late result is dropped.
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.boot_live();
        s.feed(Event::DriverFault);
        assert_eq!(s.m.phase, Phase::Paused);
        let c = s.finish_pending();
        assert!(c.is_empty(), "{c:?}");
        assert_eq!(s.m.stats.resumed, 0);
        assert!(s.m.settled.is_empty());
    }

    #[test]
    fn a_cancelled_outcome_leaves_the_run_without_settling_or_counting() {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.boot_live();
        let (k, g, a) = s.pending.pop_front().unwrap();
        assert_eq!(k, key("p", "a"));
        let c = s.feed(Event::ResumeResult {
            key: k.clone(),
            generation: g,
            attempt: a,
            outcome: ResumeOutcomeKind::Cancelled,
            load_related: false,
        });
        // No stagger: the next one starts at once.
        assert_eq!(resumes(&c), keys(&["b"]));
        assert!(!s.m.settled.contains(&k), "nothing runs: not settled");
        assert!(!queue_keys(&s.m).contains(&k));
        assert_eq!(s.m.stats.total, 1);
        assert_eq!(s.m.stats.failed, 0);
        assert_eq!(s.m.stats.resumed, 0);
        s.finish_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.settled, set(&["b"]));
    }

    #[test]
    fn late_result_of_an_attempt_a_pause_took_back_is_ignored() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.boot_live();
        assert_eq!(s.m.inflight, set(&["a", "b"]));
        let (_, gen, old_a) = s.pending[0].clone();
        // Pause with `a` in flight, then "Resume now" (no death, so the same
        // generation) resumes it again while the old worker still runs.
        s.feed(Event::DriverFault);
        s.pending.clear();
        let c = s.feed(Event::UserResumeNow);
        assert_eq!(s.m.generation, gen, "no restart, same generation");
        let restarted: Vec<u64> = c
            .iter()
            .filter_map(|c| match c {
                Command::Resume {
                    key: k, attempt, ..
                } if *k == key("p", "a") => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(restarted.len(), 1, "{c:?}");
        let new_a = restarted[0];
        assert_ne!(new_a, old_a);
        // The old worker's late result is ignored: `a` is still in flight.
        let c = s.step(Event::ResumeResult {
            key: key("p", "a"),
            generation: gen,
            attempt: old_a,
            outcome: ResumeOutcomeKind::Failed("old attempt".into()),
            load_related: true,
        });
        assert!(c.is_empty(), "{c:?}");
        assert!(s.m.inflight.contains(&key("p", "a")));
        assert_eq!(s.m.stats.failed, 0);
        assert_eq!(s.m.resume_cap, 3, "the stale result changed nothing");
        // The new attempt's result is accepted.
        s.finish_key(&key("p", "a"));
        assert!(!s.m.inflight.contains(&key("p", "a")));
        assert!(s.m.settled.contains(&key("p", "a")));
        assert_eq!(s.m.stats.resumed, 1);
    }

    // -- misc

    #[test]
    fn observed_updates_ledger_and_busy_snapshot_frozen_at_first_death() {
        let (mut m, base) = fresh();
        let o = |w: &str, busy| Observation {
            key: key("p", w),
            busy,
        };
        run(
            &mut m,
            base,
            Event::Observed(vec![o("a", true), o("b", false)]),
        );
        assert_eq!(m.ledger.len(), 2);
        run(&mut m, at(base, 5), Event::Observed(vec![o("a", true)]));
        assert_eq!(m.ledger.len(), 1, "keys missing from the list are dropped");
        assert_eq!(m.ledger[&key("p", "a")].last_busy_at, Some(at(base, 5)));
        run(
            &mut m,
            at(base, 20),
            Event::Death {
                ev: death_of(1),
                generation: Generation(2),
            },
        );
        assert_eq!(m.busy_at_death, HashSet::from([key("p", "a")]));
        // Nothing is observed while the server is down.
        run(&mut m, at(base, 21), Event::Observed(vec![o("b", true)]));
        assert!(!m.ledger.contains_key(&key("p", "b")));
        let t = pass_gate(&mut m, base, 20);
        run(&mut m, at(base, t), Event::ServerStarted(inst(2)));
        run(&mut m, at(base, t + 1), Event::Observed(vec![o("b", true)]));
        assert!(m.ledger.contains_key(&key("p", "b")));
        run(
            &mut m,
            at(base, t + 2),
            Event::Death {
                ev: death_of(2),
                generation: Generation(3),
            },
        );
        assert_eq!(
            m.busy_at_death,
            HashSet::from([key("p", "a")]),
            "frozen at the first death of the episode"
        );
    }

    #[test]
    fn explicit_stop_clears_queue_and_stays_healthy_without_server() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.boot_live();
        assert_eq!(s.m.phase, Phase::Resuming);
        let c = s.feed(Event::ExplicitStop);
        assert_eq!(records(&c), vec![(Phase::Resuming, Phase::Healthy)]);
        assert_eq!(s.m.phase, Phase::Healthy);
        assert!(s.m.queue.is_empty());
        assert!(s.m.inflight.is_empty());
        assert!(s.m.settled.is_empty());
        assert!(!s.m.server_up);
        assert_eq!(s.m.next_deadline(), None);
        assert!(!s.m.snapshot().can_resume_now);
        // The resumes that were running finish into nothing.
        assert!(s.finish_pending().is_empty());
        assert!(s.finish_pending().is_empty());

        // Also from waiting for memory.
        let mut s = Sim::new();
        s.headroom = Some(low());
        s.kill();
        s.feed(Event::ExplicitStop);
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.next_deadline(), None);
    }

    #[test]
    fn boot_recovery_does_not_count_toward_breaker_and_skips_died_notification() {
        let mut s = Sim::new();
        let c = s.feed(Event::BootRecovery {
            generation: Generation(5),
            live: None,
            bypass_gate: false,
            owed: 0,
        });
        assert_eq!(
            records(&c)[..2],
            [
                (Phase::Healthy, Phase::Down),
                (Phase::Down, Phase::AwaitHeadroom)
            ]
        );
        assert!(has_sample(&c));
        assert!(notifies(&c).is_empty());
        assert!(s.m.deaths.is_empty());
        s.to_resume = keys(&["a"]);
        let c = s.settle();
        assert!(c.contains(&Command::StartServer {
            stop_first: false,
            how: ServerStartHow::BootRecovery
        }));
        assert_eq!(s.m.phase, Phase::Healthy);
        assert!(s.m.deaths.is_empty());
        assert!(notifies(&c)
            .iter()
            .all(|n| !matches!(n, NotifyKind::Died { .. })));
        assert_eq!(
            notifies(&c),
            vec![NotifyKind::Done {
                resumed: 1,
                mid_turn: 0,
                failed: 0,
                suspended: 0
            }]
        );

        // With nothing to resume the launch is not announced at all.
        let mut s = Sim::new();
        s.feed(Event::BootRecovery {
            generation: Generation(5),
            live: None,
            bypass_gate: true,
            owed: 0,
        });
        s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert!(notifies(&s.log.iter().map(|(_, c)| c.clone()).collect::<Vec<_>>()).is_empty());
    }

    #[test]
    fn boot_recovery_with_live_instance_reconciles_without_headroom_wait() {
        let (mut m, base) = fresh();
        let c = run(
            &mut m,
            base,
            Event::BootRecovery {
                generation: Generation(4),
                live: Some(inst(9)),
                bypass_gate: false,
                owed: 0,
            },
        );
        assert!(c.contains(&Command::Reconcile {
            generation: Generation(4),
            observed: inst(9)
        }));
        assert!(!has_sample(&c) && !has_start(&c));
        assert_eq!(records(&c), vec![(Phase::Healthy, Phase::Reconciling)]);
        assert!(m.server_up);
    }

    #[test]
    fn boot_recovery_bypass_gate_starts_immediately() {
        let (mut m, base) = fresh();
        let c = run(
            &mut m,
            base,
            Event::BootRecovery {
                generation: Generation(4),
                live: None,
                bypass_gate: true,
                owed: 0,
            },
        );
        assert!(!has_sample(&c));
        assert!(c.contains(&Command::StartServer {
            stop_first: false,
            how: ServerStartHow::BootRecovery
        }));
        assert_eq!(
            records(&c),
            vec![
                (Phase::Healthy, Phase::Down),
                (Phase::Down, Phase::Restarting)
            ]
        );
    }

    #[test]
    fn adopted_outcome_goes_straight_to_healthy() {
        let (mut m, base) = fresh();
        run(
            &mut m,
            base,
            Event::BootRecovery {
                generation: Generation(4),
                live: Some(inst(9)),
                bypass_gate: false,
                owed: 0,
            },
        );
        let c = run(
            &mut m,
            at(base, 1),
            Event::ReconcileDone(Ok(ReconcileOutcome::Adopted)),
        );
        assert_eq!(records(&c), vec![(Phase::Reconciling, Phase::Healthy)]);
        assert!(m.queue.is_empty());
        assert!(notifies(&c).is_empty());
        assert!(m.server_up);
    }

    #[test]
    fn driver_fault_pauses_internal() {
        let mut s = Sim::new();
        s.headroom = Some(low());
        s.kill();
        let c = s.feed(Event::DriverFault);
        assert_eq!(s.m.pause, Some(PauseReason::Internal));
        assert_eq!(
            notifies(&c),
            vec![NotifyKind::Paused {
                reason: "Recovery stopped unexpectedly. See keitora.log.".into()
            }]
        );
        assert_eq!(s.m.next_deadline(), None);
        let c = s.feed(Event::UserResumeNow);
        assert!(has_start(&c));
    }

    #[test]
    fn start_server_only_emitted_by_machine_after_gate_pass() {
        // Starved for the whole cap: the server is never started.
        let mut s = Sim::new();
        s.headroom = Some(low());
        s.kill();
        s.settle();
        assert_eq!(s.m.pause, Some(PauseReason::HeadroomCap));
        assert!(s.starts_at().is_empty());
        // Unknown headroom degrades to "no gate" rather than a stall.
        let mut s = Sim::new();
        s.headroom = None;
        s.kill();
        s.settle();
        assert_eq!(s.starts_at(), vec![60]);
        // Every start in these scenarios went through `run`, which asserts each
        // one followed a passing sample, a user action, boot, or a retry inside a
        // bypassed episode.
        let mut s = Sim::new();
        s.kill();
        s.settle();
        assert_eq!(s.starts_at(), vec![60]);
        let mut s = Sim::new();
        s.fail_starts = 1;
        s.feed(Event::BootRecovery {
            generation: Generation(3),
            live: None,
            bypass_gate: true,
            owed: 0,
        });
        s.settle();
        assert_eq!(
            s.starts_at(),
            vec![0, 0],
            "a bypassed episode retries at once"
        );
        let mut s = Sim::new();
        s.feed(Event::UserRestart {
            generation: Generation(3),
        });
        assert_eq!(s.starts_at(), vec![0]);
    }

    // -- budget

    fn twelve() -> Vec<SessionKey> {
        (0..12).map(|i| key("p", &format!("w{i:02}"))).collect()
    }

    /// A breaker high enough that repeated deaths keep recovering; one resume
    /// at a time so the budget arithmetic is easy to follow.
    fn lenient() -> RecoveryParams {
        RecoveryParams {
            breaker_deaths: 10,
            ..serial()
        }
    }

    /// Answer `n` resumes of the current run; each answer starts the next, so
    /// one more is in flight afterwards.
    fn resume_n(s: &mut Sim, n: usize) {
        for _ in 0..n {
            s.finish_pending();
        }
    }

    /// The server dies under the run; the resumes it interrupted answer late
    /// and are ignored.
    fn kill_mid_run(s: &mut Sim) {
        s.at(s.secs() + 1);
        s.kill();
        s.drop_stale();
    }

    /// 12 sessions, 4 resumed, then a death while the 5th is in flight.
    fn death_after_four() -> Sim {
        let mut s = Sim::with(lenient());
        s.auto_resume = false;
        s.to_resume = twelve();
        s.boot_live();
        resume_n(&mut s, 4);
        assert_eq!(s.m.stats.resumed, 4);
        assert_eq!(s.pending.len(), 1);
        kill_mid_run(&mut s);
        s
    }

    #[test]
    fn death_during_resuming_halves_remaining_budget() {
        let mut s = death_after_four();
        assert_eq!(s.m.carry_budget, Some(4), "ceil(8 / 2)");
        s.auto_resume = true;
        let c = s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        // The new queue holds all 12 again; priority order wins the budget.
        assert_eq!(resumes(&c), twelve()[..4].to_vec());
        assert_eq!(s.m.stats.resumed, 4);
        assert_eq!(queue_keys(&s.m), twelve()[4..].to_vec());
        assert_eq!(
            queue_states(&s.m),
            vec![(EntryState::Suspended, Some(SuspendCause::Budget)); 8]
        );
        assert_eq!(s.m.carry_budget, None, "consumed by the queue build");
        assert_eq!(
            notifies(&c).last(),
            Some(&NotifyKind::Done {
                resumed: 4,
                mid_turn: 0,
                failed: 0,
                suspended: 8
            })
        );
        let snap = s.m.snapshot();
        assert_eq!(snap.phase, Phase::Healthy);
        assert_eq!(snap.reason_code, Some("suspended"));
        assert!(snap.can_resume_now);
        assert_eq!(snap.queue.len(), 8);
    }

    #[test]
    fn second_death_during_resuming_halves_again_min_one() {
        let mut s = death_after_four();
        assert_eq!(s.m.carry_budget, Some(4));
        // Next run: budget 4, one resumed, death while 11 are left (3 budget).
        s.settle();
        resume_n(&mut s, 1);
        kill_mid_run(&mut s);
        assert_eq!(s.m.carry_budget, Some(2), "ceil(3 / 2)");
        // Budget 2, one resumed: 1 left.
        s.settle();
        resume_n(&mut s, 1);
        kill_mid_run(&mut s);
        assert_eq!(s.m.carry_budget, Some(1));
        // Budget 1, spent on the one in flight: never below one.
        s.settle();
        assert_eq!(s.pending.len(), 1);
        kill_mid_run(&mut s);
        assert_eq!(s.m.carry_budget, Some(1));
    }

    #[test]
    fn healthy_reset_requeues_budget_suspended() {
        let mut s = death_after_four();
        s.auto_resume = true;
        s.settle();
        assert_eq!(s.m.suspended_count(), 8);
        let reset = s.m.next_deadline().unwrap();
        assert_eq!(reset, s.now + Duration::from_secs(1800));
        s.now = reset;
        s.auto_resume = false;
        let mut c = s.feed(Event::TimerFired);
        assert!(records(&c).contains(&(Phase::Healthy, Phase::Resuming)));
        assert_eq!(
            s.m.budget, 7,
            "budget = the 8 re-queued, one already started"
        );
        s.auto_resume = true;
        c.extend(s.finish_pending());
        c.extend(s.settle());
        assert_eq!(resumes(&c), twelve()[4..].to_vec());
        assert_eq!(s.m.phase, Phase::Healthy);
        assert!(s.m.queue.is_empty());
        assert!(s.m.deaths.is_empty());
    }

    // -- focus / fresh / cancel / lock

    #[test]
    fn focus_moves_entry_to_front_and_ignores_gate_and_budget() {
        let mut s = death_after_four();
        s.auto_resume = true;
        s.settle();
        assert_eq!(s.m.budget, 0);
        s.auto_resume = false;
        s.headroom = Some(low());
        let target = twelve()[9].clone();
        let c = s.feed(Event::UserFocus(target.clone()));
        assert!(!has_sample(&c), "a forced resume skips the memory check");
        assert_eq!(resumes(&c), vec![target.clone()]);
        assert_eq!(s.m.queue[0].key, target);
        assert!(s.m.queue[0].force);
        assert_eq!(s.m.budget, 0, "forced resumes do not consume budget");
        let c = s.finish_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.suspended_count(), 7, "the others stay suspended");
        assert!(
            notifies(&c).is_empty(),
            "a user-started run is not announced"
        );
    }

    #[test]
    fn focus_while_full_waits_for_a_free_slot_then_runs_next() {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c"]);
        s.boot_live();
        let c = s.feed(Event::UserFocus(key("p", "c")));
        assert!(resumes(&c).is_empty(), "the cap holds for forced entries");
        assert_eq!(queue_keys(&s.m), keys(&["c", "a", "b"]));
        s.at(10);
        // The clean result frees the slot: the focused entry takes it at once,
        // without a memory check.
        let c = s.finish_pending();
        assert_eq!(resumes(&c), keys(&["c"]));
        assert!(!has_sample(&c));
    }

    #[test]
    fn focus_in_gate_pause_resumes_one_then_returns_to_await_headroom() {
        let mut s = gate_fails_after_first();
        let c = s.feed(Event::UserFocus(key("p", "c")));
        assert!(records(&c).contains(&(Phase::AwaitHeadroom, Phase::Resuming)));
        assert_eq!(resumes(&c), keys(&["c"]));
        let c = s.finish_pending();
        assert!(records(&c).contains(&(Phase::Resuming, Phase::AwaitHeadroom)));
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
        assert_eq!(queue_keys(&s.m), keys(&["b"]));
        assert_eq!(
            queue_states(&s.m),
            vec![(EntryState::Suspended, Some(SuspendCause::Gate))]
        );

        // From a memory-cap pause the focused session starts at once.
        let mut s = gate_fails_after_first();
        s.settle();
        assert_eq!(s.m.pause, Some(PauseReason::HeadroomCap));
        let c = s.feed(Event::UserStartFresh(key("p", "b")));
        assert_eq!(resumes(&c), keys(&["b"]));
        assert_eq!(s.m.phase, Phase::Resuming);
    }

    #[test]
    fn focus_ignored_when_server_down_or_key_unknown() {
        let mut s = Sim::new();
        s.headroom = Some(low());
        s.to_resume = keys(&["a"]);
        s.kill();
        assert!(s.feed(Event::UserFocus(key("p", "a"))).is_empty());
        assert!(s.feed(Event::UserStartFresh(key("p", "a"))).is_empty());

        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.boot_live();
        assert!(s.feed(Event::UserFocus(key("p", "zzz"))).is_empty());
        assert_eq!(queue_keys(&s.m), keys(&["a", "b"]));
    }

    #[test]
    fn start_fresh_promotes_entry_with_fresh_mode() {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c"]);
        let c = s.boot_live();
        assert!(c.iter().any(|c| matches!(
            c,
            Command::Resume {
                mode: ResumeMode::Stored,
                ..
            }
        )));
        s.feed(Event::UserStartFresh(key("p", "c")));
        assert_eq!(s.m.queue[0].key, key("p", "c"));
        assert!(s.m.queue[0].fresh);
        let c = s.finish_pending();
        let gen = s.m.generation;
        assert!(c.iter().any(|c| matches!(
            c,
            Command::Resume {
                key: k,
                generation,
                mode: ResumeMode::Fresh,
                ..
            } if *k == key("p", "c") && *generation == gen
        )));
    }

    #[test]
    fn start_fresh_for_inflight_is_ignored() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.boot_live();
        assert!(s.feed(Event::UserStartFresh(key("p", "a"))).is_empty());
        assert!(!s.m.queue[0].fresh);
        assert!(s.m.inflight.contains(&key("p", "a")));
    }

    #[test]
    fn cancel_removes_queued_entry() {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c"]);
        s.boot_live();
        s.feed(Event::Cancel(key("p", "b")));
        assert_eq!(queue_keys(&s.m), keys(&["a", "c"]));
        assert_eq!(s.m.snapshot().total, 2);
        assert_eq!(resumes(&s.finish_pending()), keys(&["c"]));
    }

    #[test]
    fn cancel_inflight_marks_cancelled_and_ignores_result() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c"]);
        s.boot_live();
        s.feed(Event::Cancel(key("p", "a")));
        assert!(s.m.queue[0].cancelled);
        // Only that entry: the others in flight are untouched.
        assert_eq!(s.m.inflight, set(&["a", "b", "c"]));
        assert!(!s.m.queue[1].cancelled && !s.m.queue[2].cancelled);
        s.finish_pending();
        assert_eq!(s.m.stats.resumed, 0);
        assert_eq!(s.m.stats.total, 2);
        assert!(s.m.settled.is_empty(), "a cancelled resume never settles");
        assert_eq!(queue_keys(&s.m), keys(&["b", "c"]));
        assert_eq!(s.m.inflight, set(&["b", "c"]));
        s.finish_all_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.settled, set(&["b", "c"]));
    }

    #[test]
    fn cancel_drops_a_suspended_entry_from_the_queue_and_the_count() {
        let mut s = gate_fails_after_first();
        assert_eq!(s.m.suspended_count(), 2);
        s.feed(Event::Cancel(key("p", "b")));
        assert_eq!(queue_keys(&s.m), keys(&["c"]));
        assert_eq!(s.m.suspended_count(), 1);
        assert!(!s.m.settled.contains(&key("p", "b")));
        let snap = s.m.snapshot();
        assert_eq!(snap.queue.len(), 1);
        assert_eq!(snap.queue[0].worktree, "c");
    }

    #[test]
    fn cancel_before_queue_built_filters_it() {
        let mut s = Sim::new();
        s.headroom = Some(low());
        s.kill();
        s.feed(Event::Cancel(key("p", "b")));
        assert!(s.m.cancelled_keys.contains(&key("p", "b")));
        s.headroom = Some(ample());
        s.to_resume = keys(&["a", "b", "c"]);
        let c = s.settle();
        assert_eq!(resumes(&c), keys(&["a", "c"]));
        assert_eq!(s.m.stats.total, 2);
        assert!(s.m.cancelled_keys.is_empty());
    }

    #[test]
    fn cancel_project_removes_all_its_keys() {
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = vec![key("p", "a"), key("p", "b"), key("q", "c")];
        s.boot_live();
        assert_eq!(s.m.inflight, HashSet::from([key("p", "a")]));
        s.feed(Event::CancelProject("p".into()));
        assert_eq!(queue_keys(&s.m), vec![key("p", "a"), key("q", "c")]);
        assert!(s.m.queue[0].cancelled);
        assert_eq!(resumes(&s.finish_pending()), vec![key("q", "c")]);
        s.finish_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!((s.m.stats.resumed, s.m.stats.total), (1, 1));
    }

    #[test]
    fn cancel_last_entry_finishes_episode() {
        // The last entry is still queued: the cancel finishes the run.
        let mut s = Sim::with(serial());
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.feed(Event::TransferLock(key("p", "b"), true));
        s.kill();
        s.settle();
        s.finish_pending();
        assert_eq!(s.m.phase, Phase::Resuming, "b waits for the transfer");
        let c = s.feed(Event::Cancel(key("p", "b")));
        assert_eq!(records(&c), vec![(Phase::Resuming, Phase::Healthy)]);
        assert_eq!(
            notifies(&c),
            vec![NotifyKind::Done {
                resumed: 1,
                mid_turn: 0,
                failed: 0,
                suspended: 0
            }]
        );

        // The last entry is in flight: the run finishes when its result lands.
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.kill();
        s.settle();
        s.finish_pending();
        assert_eq!(s.m.phase, Phase::Resuming);
        let c = s.feed(Event::Cancel(key("p", "b")));
        assert!(
            records(&c).is_empty(),
            "finish waits for the work in flight"
        );
        assert_eq!(s.m.phase, Phase::Resuming);
        let c = s.finish_pending();
        assert_eq!(records(&c), vec![(Phase::Resuming, Phase::Healthy)]);
        assert_eq!(
            notifies(&c),
            vec![NotifyKind::Done {
                resumed: 1,
                mid_turn: 0,
                failed: 0,
                suspended: 0
            }]
        );
    }

    #[test]
    fn transfer_lock_skips_key_and_resumes_others() {
        let mut s = Sim::new();
        s.feed(Event::TransferLock(key("p", "b"), true));
        s.to_resume = keys(&["a", "b", "c"]);
        s.boot_live();
        s.settle();
        assert_eq!(
            resumes(&s.log.iter().map(|(_, c)| c.clone()).collect::<Vec<_>>()),
            keys(&["a", "c"])
        );
        assert_eq!(s.m.phase, Phase::Resuming, "waits for the unlock");
        assert_eq!(queue_keys(&s.m), keys(&["b"]));
        assert_eq!(queue_states(&s.m), vec![(EntryState::Queued, None)]);
        assert_eq!(s.m.next_deadline(), None);
    }

    #[test]
    fn unlock_resumes_skipped_key() {
        let mut s = Sim::new();
        s.feed(Event::TransferLock(key("p", "b"), true));
        s.to_resume = keys(&["a", "b", "c"]);
        s.boot_live();
        s.settle();
        s.at(s.secs() + 60);
        let mut c = s.feed(Event::TransferLock(key("p", "b"), false));
        c.extend(s.settle());
        assert_eq!(resumes(&c), keys(&["b"]));
        assert_eq!(s.m.phase, Phase::Healthy);
        assert!(s.m.locks.is_empty());
    }

    // -- several in flight: failure paths

    #[test]
    fn memory_failure_with_work_in_flight_drains_before_waiting() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d", "e"]);
        // Enough for two at once.
        s.headroom = Some(snap_mib(5296, 64 * 1024));
        s.boot_live();
        assert_eq!(s.m.inflight, set(&["a", "b"]));
        s.headroom = Some(low());
        s.at(10);
        // `a` lands; the check for one more fails with `b` still running:
        // nothing is parked, the queue waits for `b`'s result.
        let c = s.finish_key(&key("p", "a"));
        assert!(has_sample(&c));
        assert!(records(&c).is_empty(), "stays in Resuming: {c:?}");
        assert_eq!(s.m.phase, Phase::Resuming);
        assert_eq!(
            queue_states(&s.m),
            vec![
                (EntryState::Resuming, None),
                (EntryState::Queued, None),
                (EntryState::Queued, None),
                (EntryState::Queued, None),
            ]
        );
        // The last one in flight drains, the check fails again with nothing
        // running, and only then the rest is parked while recovery waits for
        // memory.
        let c = s.finish_key(&key("p", "b"));
        assert!(records(&c).contains(&(Phase::Resuming, Phase::AwaitHeadroom)));
        assert!(timings(&c).is_empty(), "not finished yet");
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
        assert_eq!(s.m.settled, set(&["a", "b"]));
        assert_eq!(queue_keys(&s.m), keys(&["c", "d", "e"]));
    }

    #[test]
    fn failing_check_with_work_in_flight_parks_nothing_and_retries_on_the_next_result() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d", "e"]);
        s.boot_live();
        assert_eq!(s.m.resume_cap, 3);
        assert_eq!(s.m.inflight, set(&["a", "b", "c"]));
        s.headroom = Some(low());
        s.at(10);
        // `a` lands; the check for `d` fails while `b` and `c` still run.
        let c = s.finish_key(&key("p", "a"));
        assert!(has_sample(&c));
        assert!(resumes(&c).is_empty());
        assert!(records(&c).is_empty(), "stays in Resuming: {c:?}");
        assert_eq!(s.m.suspended_count(), 0, "nothing parked");
        assert_eq!(
            queue_states(&s.m)[2..],
            [(EntryState::Queued, None), (EntryState::Queued, None)]
        );
        // `b` lands: the next check fails too, still nothing parked.
        let c = s.finish_key(&key("p", "b"));
        assert!(has_sample(&c));
        assert!(resumes(&c).is_empty());
        assert_eq!(s.m.suspended_count(), 0);
        assert_eq!(s.m.phase, Phase::Resuming);
        // Memory is back when `c` lands: the passing sample starts `d` and `e`.
        s.headroom = Some(ample());
        let c = s.finish_key(&key("p", "c"));
        assert!(has_sample(&c));
        assert_eq!(resumes(&c), keys(&["d", "e"]));
        assert_eq!(s.m.phase, Phase::Resuming);
        s.finish_all_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.stats.resumed, 5);

        // A failing check with nothing in flight still parks the rest and
        // waits for memory.
        let s = gate_fails_after_first();
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
        assert_eq!(
            queue_states(&s.m),
            vec![(EntryState::Suspended, Some(SuspendCause::Gate)); 2]
        );
    }

    #[test]
    fn pause_requeues_every_resume_in_flight() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d"]);
        s.boot_live();
        assert_eq!(s.m.inflight.len(), 3);
        s.feed(Event::DriverFault);
        assert_eq!(s.m.phase, Phase::Paused);
        assert!(s.m.inflight.is_empty());
        assert_eq!(queue_keys(&s.m), keys(&["a", "b", "c", "d"]));
        assert_eq!(queue_states(&s.m), vec![(EntryState::Queued, None); 4]);
        // Their late results are dropped.
        assert!(s.finish_all_pending().is_empty());
        assert_eq!(s.m.stats.resumed, 0);
    }

    #[test]
    fn death_with_three_in_flight_accounts_for_all_of_them() {
        let mut s = Sim::with(RecoveryParams {
            breaker_deaths: 10,
            ..RecoveryParams::default()
        });
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d", "e", "f"]);
        s.boot_live();
        assert_eq!(s.m.inflight, set(&["a", "b", "c"]));
        assert_eq!(s.m.budget, 3);
        s.at(5);
        s.kill();
        // One breaker death, however many resumes it interrupted.
        assert_eq!(s.m.deaths.len(), 1);
        // All three in flight still owed their share: min(6, 3 + 3) halved.
        assert_eq!(s.m.carry_budget, Some(3));
        assert!(s.m.inflight.is_empty() && s.m.queue.is_empty());
        // Their late results carry the old generation and are dropped.
        s.drop_stale();
        assert_eq!((s.m.stats.resumed, s.m.stats.failed), (0, 0));
        // The next run owes all three again, first in line.
        s.auto_resume = true;
        let c = s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(resumes(&c), keys(&["a", "b", "c"]));
        assert_eq!(s.m.suspended_count(), 3);
    }

    #[test]
    fn death_after_one_of_two_results_halves_the_next_run() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b"]);
        s.failing.insert(key("p", "a"));
        s.boot_live();
        assert_eq!(s.m.inflight, set(&["a", "b"]));
        // `a` fails against the dying server and its result lands before the
        // death; `b` saw the generation move on and never answers.
        s.finish_key(&key("p", "a"));
        assert_eq!(s.m.inflight, set(&["b"]));
        s.at(5);
        s.kill();
        // Only `b` was left, already charged: min(1, 0 + 1) halved.
        assert_eq!(s.m.carry_budget, Some(1));
        assert!(s.m.inflight.is_empty() && s.m.queue.is_empty());
        s.pending.clear();
        // The next run owes both again but resumes one; the other waits.
        s.failing.clear();
        s.auto_resume = true;
        let c = s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(resumes(&c), keys(&["a"]));
        assert_eq!(s.m.suspended_count(), 1);
    }

    // -- settled

    #[test]
    fn settled_holds_successful_resumes_including_already_running() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c", "d"]);
        s.running.insert(key("p", "b"));
        s.failing.insert(key("p", "c"));
        s.boot_live();
        assert!(s.m.settled.is_empty(), "nothing settles before its result");
        s.finish_key(&key("p", "a"));
        assert_eq!(s.m.settled, set(&["a"]));
        s.finish_key(&key("p", "b"));
        s.finish_key(&key("p", "c"));
        assert_eq!(s.m.settled, set(&["a", "b"]), "a failed key never settles");
        // Queued `d` is not settled; it settles when it resumes.
        assert!(!s.m.settled.contains(&key("p", "d")));
        s.finish_all_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        // The finish keeps them.
        assert_eq!(s.m.settled, set(&["a", "b", "d"]));
        // A cancel drops the key.
        s.feed(Event::Cancel(key("p", "a")));
        assert_eq!(s.m.settled, set(&["b", "d"]));
    }

    #[test]
    fn settled_on_the_wire_sorted() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = vec![key("q", "a"), key("p", "z"), key("p", "b")];
        s.boot_live();
        s.finish_all_pending();
        let snap = s.m.snapshot();
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(
            v["settled"],
            serde_json::json!([
                { "project": "p", "worktree": "b" },
                { "project": "p", "worktree": "z" },
                { "project": "q", "worktree": "a" },
            ])
        );
    }

    #[test]
    fn settled_cleared_when_an_episode_starts_or_recovery_stops() {
        let settled_run = || {
            let mut s = Sim::new();
            s.to_resume = keys(&["a", "b"]);
            s.boot_live();
            assert_eq!(s.m.phase, Phase::Healthy);
            assert_eq!(s.m.settled, set(&["a", "b"]));
            s.to_resume = Vec::new();
            s.at(10);
            s
        };
        let mut s = settled_run();
        s.kill();
        assert!(s.m.settled.is_empty(), "death");
        let mut s = settled_run();
        s.boot_live();
        assert!(s.m.settled.is_empty(), "boot");
        let mut s = settled_run();
        s.feed(Event::UserRestart {
            generation: Generation(9),
        });
        assert!(s.m.settled.is_empty(), "user restart");
        let mut s = settled_run();
        s.feed(Event::ExplicitStop);
        assert!(s.m.settled.is_empty(), "explicit stop");
    }

    #[test]
    fn healthy_reset_run_keeps_earlier_settled_keys() {
        let mut s = death_after_four();
        s.auto_resume = true;
        s.settle();
        assert_eq!(s.m.settled.len(), 4);
        s.now = s.m.next_deadline().unwrap();
        s.feed(Event::TimerFired);
        s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.settled.len(), 12);
    }

    // -- boot sustain skip

    #[test]
    fn boot_recovery_carries_owed_only_for_a_gated_down_launch() {
        let boot = |live: Option<ServerInstance>, bypass_gate: bool| {
            let (mut m, base) = fresh();
            run(
                &mut m,
                base,
                Event::BootRecovery {
                    generation: Generation(2),
                    live,
                    bypass_gate,
                    owed: 5,
                },
            );
            m.episode.boot_owed
        };
        assert_eq!(boot(None, false), 5);
        assert_eq!(boot(None, true), 0);
        assert_eq!(boot(Some(inst(9)), false), 0);
    }

    #[test]
    fn boot_sample_ample_for_the_whole_queue_skips_the_sustain() {
        let mut s = Sim::new();
        s.to_resume = keys(&["a", "b", "c"]);
        // Commit for exactly three sessions at once.
        s.headroom = Some(snap_mib(5896, 64 * 1024));
        let c = s.boot_down(3);
        assert!(has_start(&c));
        assert_eq!(s.starts_at(), vec![0]);
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(s.m.stats.resumed, 3);
    }

    #[test]
    fn boot_sample_enough_for_one_but_not_the_queue_keeps_the_sustain() {
        let mut s = Sim::new();
        s.to_resume = keys(&["a", "b", "c"]);
        // Enough for two at once, not three.
        s.headroom = Some(snap_mib(5296, 64 * 1024));
        s.boot_down(3);
        assert_eq!(s.m.phase, Phase::AwaitHeadroom);
        s.settle();
        assert_eq!(s.starts_at(), vec![60]);
        assert!(s.m.episode.boot_owed == 0, "the episode is over");

        // A launch that owes nothing waits the full sustain too.
        let mut s = Sim::new();
        s.boot_down(0);
        s.settle();
        assert_eq!(s.starts_at(), vec![60]);
    }

    #[test]
    fn only_the_first_boot_sample_may_skip_the_sustain() {
        let mut s = Sim::new();
        s.to_resume = keys(&["a"]);
        s.headroom = Some(low());
        s.boot_down(1);
        assert!(s.m.episode.boot_sustain_checked);
        // Memory frees up at the next sample: the normal sustain runs from there.
        s.headroom = Some(ample());
        s.settle();
        // Samples at 0, 5, 10, 20, 30, 45, 60, 90: stable from 5, 60 s at 90.
        assert_eq!(s.starts_at(), vec![90]);
    }

    #[test]
    fn a_death_never_skips_the_sustain() {
        let mut s = Sim::new();
        s.to_resume = keys(&["a"]);
        s.kill();
        assert!(!s.m.episode.boot);
        s.settle();
        assert_eq!(s.starts_at(), vec![60]);
    }

    // -- timing

    #[test]
    fn recovery_timing_recorded_once_when_a_resuming_run_turns_healthy() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a", "b", "c"]);
        // `c` times out.
        s.failing.insert(key("p", "c"));
        s.load.insert(key("p", "c"));
        s.kill();
        s.settle();
        assert_eq!(s.secs(), 60);
        assert_eq!(s.m.inflight.len(), 3);
        s.at(64);
        let c = s.finish_all_pending();
        assert_eq!(s.m.phase, Phase::Healthy);
        assert_eq!(
            timings(&c),
            vec![Command::RecordTiming {
                sustain_ms: 60_000,
                resume_ms: 4_000,
                sessions: 3,
                failed: 1,
                final_cap: 1,
            }]
        );
        let all: Vec<Command> = s.log.iter().map(|(_, c)| c.clone()).collect();
        assert_eq!(timings(&all).len(), 1);
        // Recorded in the step that reached Healthy, before the emit.
        let step = s
            .log
            .iter()
            .position(|(_, c)| matches!(c, Command::RecordTiming { .. }))
            .unwrap();
        assert!(matches!(
            s.log[step - 1].1,
            Command::Notify(NotifyKind::Done { .. })
        ));
        assert_eq!(
            s.m.resume_cap, 3,
            "the cap is restored for the next episode"
        );
    }

    #[test]
    fn no_recovery_timing_without_a_resume_phase() {
        let mut s = Sim::new();
        s.kill();
        s.settle();
        assert_eq!(s.m.phase, Phase::Healthy);
        let all: Vec<Command> = s.log.iter().map(|(_, c)| c.clone()).collect();
        assert!(timings(&all).is_empty());

        // Without a memory wait the sustain counts as zero.
        let mut s = Sim::new();
        s.to_resume = keys(&["a"]);
        let c = s.boot_live();
        assert_eq!(
            timings(&c),
            vec![Command::RecordTiming {
                sustain_ms: 0,
                resume_ms: 0,
                sessions: 1,
                failed: 0,
                final_cap: 3,
            }]
        );
    }

    #[test]
    fn shutdown_event_is_ignored_by_the_machine() {
        let mut s = Sim::new();
        s.auto_resume = false;
        s.to_resume = keys(&["a"]);
        s.boot_live();
        assert!(s.feed(Event::Shutdown).is_empty());
        assert_eq!(s.m.phase, Phase::Resuming);
    }
}

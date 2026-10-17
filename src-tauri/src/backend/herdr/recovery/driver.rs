//! The recovery driver: the one thread that owns the [`RecoveryMachine`], feeds
//! it events and executes the commands it returns against the world.
//!
//! Commands run in order, on the driver thread, blocking, except resumes: each
//! one runs on a short-lived worker thread (the machine bounds how many are in
//! flight) and its outcome comes back through the driver's own channel as an
//! [`Event::ResumeResult`]. Events that arrive meanwhile wait in the channel.
//! Any other command's answer (a headroom sample, a started server) is fed
//! straight back to the machine in the same turn.
//!
//! Before any command of a step runs, the driver publishes the machine's phase,
//! in-flight keys and settled keys into the gate cell and refreshes the cached payload, so the
//! gate already admits a resume when the executor launches it.

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::backend::SessionKey;
use crate::headroom::HeadroomSnapshot;
use crate::lifecycle::{self, LifecycleEvent, ServerStartHow};

use super::machine::{render, Command, Event, RecoveryMachine, ResumeMode, ResumeOutcomeKind};
use super::payload::RecoveryPayload;
use super::GateView;
use crate::backend::herdr::ops::{Generation, ServerInstance};
use crate::backend::herdr::reconcile::ReconcileOutcome;

/// How one resume ended, as the runner saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeReport {
    pub outcome: ResumeOutcomeKind,
    /// The resume timed out, or its agent exited early and a weaker decision
    /// was used: evidence that the machine is overloaded.
    pub load_related: bool,
}

/// Runs one session's resume. Called on a worker thread, so several may run at
/// once (never two for the same key: the machine holds one entry per key).
pub trait ResumeRunner: Send + Sync + 'static {
    /// `None`: skipped, because the generation moved on before the resume
    /// touched anything (a death or restart superseded it).
    fn resume(
        &self,
        key: &SessionKey,
        generation: Generation,
        mode: ResumeMode,
    ) -> Option<ResumeReport>;
}

/// Where the driver runs a resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeDispatch {
    /// On a `herdr-resume` worker thread; the outcome is sent on the driver's
    /// channel.
    Threads,
    /// On the driver thread, the outcome fed back in the same turn (tests).
    Inline,
}

/// Everything the driver does to the world. Implementations never send events
/// to the machine themselves: answers come back as return values (a threaded
/// resume's through the driver's channel, see [`ResumeDispatch`]).
pub trait Executor: Send {
    fn sample_headroom(&mut self) -> Option<HeadroomSnapshot>;
    /// Start the server (stopping the running one first when `stop_first`) and
    /// wait until it answers.
    fn start_server(
        &mut self,
        stop_first: bool,
        how: ServerStartHow,
    ) -> Result<ServerInstance, String>;
    fn reconcile(
        &mut self,
        generation: Generation,
        observed: ServerInstance,
    ) -> Result<ReconcileOutcome, String>;
    /// The runner every resume goes through.
    fn resume_runner(&self) -> Arc<dyn ResumeRunner>;
    /// Show an OS notification.
    fn notify(&mut self, title: &str, body: &str);
    /// Push `daemon://recovery`.
    fn emit(&mut self, payload: &RecoveryPayload);
}

/// The clock the driver reads; injected so tests control time.
pub type Clock = Box<dyn Fn() -> Instant + Send>;

/// The machine plus the world it drives.
pub struct Driver {
    machine: RecoveryMachine,
    exec: Box<dyn Executor>,
    runner: Arc<dyn ResumeRunner>,
    view: Arc<RwLock<GateView>>,
    cache: Arc<RwLock<RecoveryPayload>>,
    clock: Clock,
    /// Where resume workers send their outcome (the driver's own channel).
    tx: Sender<Event>,
    dispatch: ResumeDispatch,
}

impl Driver {
    pub fn new(
        machine: RecoveryMachine,
        exec: Box<dyn Executor>,
        view: Arc<RwLock<GateView>>,
        cache: Arc<RwLock<RecoveryPayload>>,
        clock: Clock,
        tx: Sender<Event>,
        dispatch: ResumeDispatch,
    ) -> Self {
        let runner = exec.resume_runner();
        Driver {
            machine,
            exec,
            runner,
            view,
            cache,
            clock,
            tx,
            dispatch,
        }
    }

    /// How long to wait for the next event before the machine's next timer is
    /// due (`None`: no timer, wait indefinitely).
    pub fn timeout(&self) -> Option<Duration> {
        let now = (self.clock)();
        self.machine
            .next_deadline()
            .map(|d| d.saturating_duration_since(now))
    }

    /// Feed one event and run everything that follows from it. A panic anywhere
    /// in that turn pauses recovery instead of killing the driver (a dead driver
    /// would leave the gate stuck in whatever phase it last published).
    pub fn feed(&mut self, ev: Event) {
        if panic::catch_unwind(AssertUnwindSafe(|| self.process(ev))).is_ok() {
            return;
        }
        log::error!("herdr: the recovery driver failed; pausing recovery");
        if panic::catch_unwind(AssertUnwindSafe(|| self.process(Event::DriverFault))).is_err() {
            log::error!("herdr: the recovery driver failed again while pausing");
        }
    }

    fn process(&mut self, ev: Event) {
        let mut queue = VecDeque::new();
        self.step_into(ev, &mut queue);
        while let Some(cmd) = queue.pop_front() {
            if let Some(answer) = self.execute(cmd) {
                self.step_into(answer, &mut queue);
            }
        }
    }

    /// Step the machine, publish its view, and queue its commands.
    fn step_into(&mut self, ev: Event, queue: &mut VecDeque<Command>) {
        let now = (self.clock)();
        let cmds = self.machine.step(ev, now);
        self.publish();
        for cmd in cmds {
            if cmd == Command::EmitRecovery {
                // Emitted with this step's payload, before any slow command
                // queued ahead of it runs: every phase reaches the frontend.
                self.execute(cmd);
            } else {
                queue.push_back(cmd);
            }
        }
    }

    fn publish(&self) {
        let view = GateView {
            phase: self.machine.phase(),
            inflight: self.machine.inflight().clone(),
            settled: self.machine.settled().clone(),
        };
        let payload = self.machine.snapshot();
        // The payload first: whoever sees the gate move also reads the new payload.
        if let Ok(mut c) = self.cache.write() {
            *c = payload;
        }
        if let Ok(mut v) = self.view.write() {
            *v = view;
        }
    }

    /// Run one command; `Some` is the answer to feed back.
    fn execute(&mut self, cmd: Command) -> Option<Event> {
        match cmd {
            Command::SampleHeadroom => Some(Event::HeadroomTick(self.exec.sample_headroom())),
            Command::StartServer { stop_first, how } => {
                Some(match self.exec.start_server(stop_first, how) {
                    Ok(instance) => Event::ServerStarted(instance),
                    Err(e) => Event::ServerStartFailed(e),
                })
            }
            Command::Reconcile {
                generation,
                observed,
            } => Some(Event::ReconcileDone(
                self.exec.reconcile(generation, observed),
            )),
            Command::Resume {
                key,
                generation,
                mode,
                attempt,
            } => match self.dispatch {
                ResumeDispatch::Inline => run_resume(&*self.runner, key, generation, mode, attempt),
                ResumeDispatch::Threads => self.spawn_resume(key, generation, mode, attempt),
            },
            Command::Notify(kind) => {
                let (title, body) = render(&kind);
                self.exec.notify(&title, &body);
                None
            }
            Command::EmitRecovery => {
                let payload = self.cache.read().map(|c| c.clone()).ok();
                if let Some(payload) = payload {
                    self.exec.emit(&payload);
                }
                None
            }
            Command::Record { from, to, reason } => {
                lifecycle::record(LifecycleEvent::RecoveryPhase {
                    from: from.as_str(),
                    to: to.as_str(),
                    reason,
                });
                None
            }
            Command::RecordTiming {
                sustain_ms,
                resume_ms,
                sessions,
                failed,
                final_cap,
            } => {
                lifecycle::record(LifecycleEvent::RecoveryTiming {
                    sustain_ms,
                    resume_ms,
                    sessions,
                    failed,
                    final_cap,
                });
                None
            }
        }
    }

    /// Run one resume on a worker thread that sends its outcome (tagged with
    /// `attempt`) on the driver's channel; nothing to feed back now. A worker
    /// that cannot be started fails the resume at once.
    fn spawn_resume(
        &self,
        key: SessionKey,
        generation: Generation,
        mode: ResumeMode,
        attempt: u64,
    ) -> Option<Event> {
        let runner = Arc::clone(&self.runner);
        let tx = self.tx.clone();
        let worker_key = key.clone();
        let spawned = std::thread::Builder::new()
            .name("herdr-resume".into())
            .spawn(move || {
                if let Some(ev) = run_resume(&*runner, worker_key, generation, mode, attempt) {
                    // A driver that is gone no longer wants the outcome.
                    let _ = tx.send(ev);
                }
            });
        match spawned {
            Ok(_) => None,
            Err(e) => {
                log::error!("herdr: could not start a resume worker for {key}: {e}");
                Some(Event::ResumeResult {
                    key,
                    generation,
                    attempt,
                    outcome: ResumeOutcomeKind::Failed(format!("could not start the resume: {e}")),
                    load_related: false,
                })
            }
        }
    }

    /// Drive until `recv` returns `None` or [`Event::Shutdown`]. `recv` gets
    /// the wait before the next timer (`None` = no timer) and answers an
    /// event, [`Event::TimerFired`] when that wait ran out, or `None` to stop.
    pub fn run_with(mut self, mut recv: impl FnMut(Option<Duration>) -> Option<Event>) {
        while let Some(ev) = recv(self.timeout()) {
            if matches!(ev, Event::Shutdown) {
                break;
            }
            self.feed(ev);
        }
    }

    /// Drive from `rx` until [`Event::Shutdown`] or every sender is gone. The
    /// driver holds a sender itself (for its resume workers), so a shutdown is
    /// what normally ends it.
    pub fn run(self, rx: Receiver<Event>) {
        self.run_with(|timeout| match timeout {
            None => rx.recv().ok(),
            Some(t) => match rx.recv_timeout(t) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => Some(Event::TimerFired),
                Err(RecvTimeoutError::Disconnected) => None,
            },
        });
    }

    #[cfg(test)]
    pub fn machine(&self) -> &RecoveryMachine {
        &self.machine
    }
}

/// Run one resume through `runner` and turn its report into the machine's
/// answer, tagged with the `attempt` the machine started (`None`: skipped,
/// nothing to answer). A panicking runner fails the resume instead of taking
/// the worker (or the driver) down with it.
fn run_resume(
    runner: &dyn ResumeRunner,
    key: SessionKey,
    generation: Generation,
    mode: ResumeMode,
    attempt: u64,
) -> Option<Event> {
    let ran = panic::catch_unwind(AssertUnwindSafe(|| runner.resume(&key, generation, mode)));
    let report = match ran {
        Ok(report) => report?,
        Err(_) => {
            log::error!("herdr: resuming {key} panicked");
            ResumeReport {
                outcome: ResumeOutcomeKind::Failed("resume panicked".into()),
                load_related: false,
            }
        }
    };
    Some(Event::ResumeResult {
        key,
        generation,
        attempt,
        outcome: report.outcome,
        load_related: report.load_related,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::machine::{Phase, RecoveryParams};
    use super::super::{Notifier, RecoveryGate};
    use super::*;
    use crate::backend::herdr::gate::{GateRefusal, ServerGate};
    use crate::backend::herdr::watchdog::{DeathEvent, DeathSource};
    use crate::headroom::MIB;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::time::SystemTime;

    /// Every notification shown, as `(title, body)`.
    #[derive(Default)]
    pub(crate) struct RecordingNotifier(pub Mutex<Vec<(String, String)>>);

    impl Notifier for RecordingNotifier {
        fn notify(&self, title: &str, body: &str) {
            self.0
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
        }
    }

    impl RecordingNotifier {
        pub(crate) fn titles(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(t, _)| t.clone())
                .collect()
        }
    }

    /// What the scripted executor was asked, in order.
    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Sample,
        Start(bool),
        Reconcile(Generation),
        Resume(SessionKey, ResumeMode),
        Notify(String),
        Emit(Phase),
    }

    /// How the scripted runner answers a resume.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Behaviour {
        /// The session comes back.
        Resume,
        /// The generation moved on: skipped, nothing reported.
        Skip,
        /// The runner panics.
        Panic,
    }

    /// The scripted resume runner; records each resume and the gate's answer
    /// for its key at that moment.
    struct FakeRunner {
        calls: Arc<Mutex<Vec<Call>>>,
        view: Arc<RwLock<GateView>>,
        admitted: Arc<Mutex<Vec<Result<(), GateRefusal>>>>,
        behaviour: Mutex<Behaviour>,
    }

    impl ResumeRunner for FakeRunner {
        fn resume(
            &self,
            key: &SessionKey,
            _generation: Generation,
            mode: ResumeMode,
        ) -> Option<ResumeReport> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Resume(key.clone(), mode));
            let behaviour = *self.behaviour.lock().unwrap();
            match behaviour {
                Behaviour::Resume => {}
                Behaviour::Skip => return None,
                Behaviour::Panic => panic!("resume blew up"),
            }
            let gate = RecoveryGate::from_view(Arc::clone(&self.view));
            self.admitted.lock().unwrap().push(gate.admit(Some(key)));
            Some(ResumeReport {
                outcome: ResumeOutcomeKind::Resumed,
                load_related: false,
            })
        }
    }

    /// A scripted world. Answers default to: ample memory, servers that start
    /// with increasing pids, a reconcile owing `to_resume`, resumes that work.
    struct FakeExecutor {
        calls: Arc<Mutex<Vec<Call>>>,
        headroom: VecDeque<Option<HeadroomSnapshot>>,
        to_resume: Vec<SessionKey>,
        pid: u32,
        notifier: Arc<RecordingNotifier>,
        runner: Arc<FakeRunner>,
        panic_on_reconcile: bool,
    }

    impl Executor for FakeExecutor {
        fn sample_headroom(&mut self) -> Option<HeadroomSnapshot> {
            self.calls.lock().unwrap().push(Call::Sample);
            self.headroom.pop_front().unwrap_or(Some(ample()))
        }
        fn start_server(
            &mut self,
            stop_first: bool,
            _how: ServerStartHow,
        ) -> Result<ServerInstance, String> {
            self.calls.lock().unwrap().push(Call::Start(stop_first));
            self.pid += 1;
            Ok(inst(self.pid))
        }
        fn reconcile(
            &mut self,
            generation: Generation,
            _observed: ServerInstance,
        ) -> Result<ReconcileOutcome, String> {
            self.calls.lock().unwrap().push(Call::Reconcile(generation));
            if self.panic_on_reconcile {
                self.panic_on_reconcile = false;
                panic!("reconcile blew up");
            }
            Ok(ReconcileOutcome::NewGeneration {
                to_resume: self.to_resume.clone(),
            })
        }
        fn resume_runner(&self) -> Arc<dyn ResumeRunner> {
            Arc::clone(&self.runner) as Arc<dyn ResumeRunner>
        }
        fn notify(&mut self, title: &str, body: &str) {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Notify(title.to_string()));
            self.notifier.notify(title, body);
        }
        fn emit(&mut self, payload: &RecoveryPayload) {
            self.calls.lock().unwrap().push(Call::Emit(payload.phase));
        }
    }

    fn ample() -> HeadroomSnapshot {
        HeadroomSnapshot {
            commit_avail: 64 * 1024 * MIB,
            phys_avail: 64 * 1024 * MIB,
            pressure: None,
        }
    }

    fn low() -> HeadroomSnapshot {
        HeadroomSnapshot {
            commit_avail: 1024 * MIB,
            phys_avail: 64 * 1024 * MIB,
            pressure: None,
        }
    }

    fn inst(pid: u32) -> ServerInstance {
        ServerInstance {
            pid,
            start_time: u64::from(pid),
        }
    }

    fn key(w: &str) -> SessionKey {
        SessionKey {
            project: "p".into(),
            worktree: w.into(),
        }
    }

    fn death_of(pid: u32) -> DeathEvent {
        DeathEvent {
            exit_code: Some(-1073740791),
            signal: None,
            source: DeathSource::Child,
            uptime: Duration::from_secs(3600),
            headroom_at_death: None,
            instance: inst(pid),
            detected_at: SystemTime::UNIX_EPOCH,
        }
    }

    /// A driver over a scripted executor with a hand-moved clock.
    struct Rig {
        driver: Driver,
        now: Arc<Mutex<Instant>>,
        base: Instant,
        calls: Arc<Mutex<Vec<Call>>>,
        notifier: Arc<RecordingNotifier>,
        view: Arc<RwLock<GateView>>,
        cache: Arc<RwLock<RecoveryPayload>>,
        admitted: Arc<Mutex<Vec<Result<(), GateRefusal>>>>,
        runner: Arc<FakeRunner>,
        /// The driver's channel: threaded resume workers send their results here.
        rx: Receiver<Event>,
    }

    fn rig(
        to_resume: Vec<SessionKey>,
        headroom: Vec<Option<HeadroomSnapshot>>,
        panic_on_reconcile: bool,
    ) -> Rig {
        rig_with(
            to_resume,
            headroom,
            panic_on_reconcile,
            ResumeDispatch::Inline,
        )
    }

    fn rig_with(
        to_resume: Vec<SessionKey>,
        headroom: Vec<Option<HeadroomSnapshot>>,
        panic_on_reconcile: bool,
        dispatch: ResumeDispatch,
    ) -> Rig {
        let base = Instant::now();
        let now = Arc::new(Mutex::new(base));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let notifier = Arc::new(RecordingNotifier::default());
        let view = Arc::new(RwLock::new(GateView::default()));
        let cache = Arc::new(RwLock::new(RecoveryPayload::healthy()));
        let admitted = Arc::new(Mutex::new(Vec::new()));
        let runner = Arc::new(FakeRunner {
            calls: Arc::clone(&calls),
            view: Arc::clone(&view),
            admitted: Arc::clone(&admitted),
            behaviour: Mutex::new(Behaviour::Resume),
        });
        let exec = FakeExecutor {
            calls: Arc::clone(&calls),
            headroom: headroom.into(),
            to_resume,
            pid: 1,
            notifier: Arc::clone(&notifier),
            runner: Arc::clone(&runner),
            panic_on_reconcile,
        };
        let clock_now = Arc::clone(&now);
        let (tx, rx) = std::sync::mpsc::channel();
        let driver = Driver::new(
            RecoveryMachine::new(RecoveryParams::default(), base),
            Box::new(exec),
            Arc::clone(&view),
            Arc::clone(&cache),
            Box::new(move || *clock_now.lock().unwrap()),
            tx,
            dispatch,
        );
        Rig {
            driver,
            now,
            base,
            calls,
            notifier,
            view,
            cache,
            admitted,
            runner,
            rx,
        }
    }

    impl Rig {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn phase(&self) -> Phase {
            self.driver.machine().phase()
        }

        /// Move the clock to the next timer and fire it, until the machine
        /// settles or has no timer left.
        fn settle(&mut self) {
            for _ in 0..1000 {
                if matches!(self.phase(), Phase::Healthy | Phase::Paused) {
                    return;
                }
                let Some(d) = self.driver.machine().next_deadline() else {
                    return;
                };
                {
                    let mut now = self.now.lock().unwrap();
                    if d > *now {
                        *now = d;
                    }
                }
                self.driver.feed(Event::TimerFired);
            }
            panic!("the machine never settled");
        }

        fn observe(&mut self, keys: &[SessionKey]) {
            self.driver.feed(Event::Observed(
                keys.iter()
                    .map(|k| super::super::machine::Observation {
                        key: k.clone(),
                        busy: false,
                    })
                    .collect(),
            ));
        }
    }

    fn without_emits(calls: Vec<Call>) -> Vec<Call> {
        calls
            .into_iter()
            .filter(|c| !matches!(c, Call::Emit(_)))
            .collect()
    }

    #[test]
    fn driver_executes_commands_in_order_and_feeds_results_back() {
        let mut r = rig(vec![key("a"), key("b")], Vec::new(), false);
        r.observe(&[key("a"), key("b")]);
        r.driver.feed(Event::Death {
            ev: death_of(1),
            generation: Generation(2),
        });
        r.settle();
        assert_eq!(r.phase(), Phase::Healthy);
        let calls = without_emits(r.calls());
        // The first death notifies, then waits for a sustained memory pass.
        assert_eq!(calls[0], Call::Notify("herdr stopped".into()));
        let first_start = calls
            .iter()
            .position(|c| matches!(c, Call::Start(_)))
            .expect("a start");
        assert!(first_start > 1);
        assert!(calls[1..first_start].iter().all(|c| *c == Call::Sample));
        assert_eq!(
            calls[first_start..],
            [
                Call::Start(false),
                Call::Reconcile(Generation(2)),
                // One sample admits both: memory covers two at once.
                Call::Sample,
                Call::Resume(key("a"), ResumeMode::Stored),
                Call::Resume(key("b"), ResumeMode::Stored),
                Call::Notify("Sessions resumed".into()),
            ]
        );
        // The last payload emitted reflects the final phase.
        let last_emit = r
            .calls()
            .into_iter()
            .rev()
            .find(|c| matches!(c, Call::Emit(_)));
        assert_eq!(last_emit, Some(Call::Emit(Phase::Healthy)));
    }

    #[test]
    fn sample_headroom_result_is_fed_in_same_turn() {
        let mut r = rig(Vec::new(), vec![Some(low())], false);
        r.driver.feed(Event::Death {
            ev: death_of(1),
            generation: Generation(2),
        });
        // One feed: the death asked for a sample, the failing answer went straight
        // back in and the machine already announced the wait for memory.
        assert_eq!(r.phase(), Phase::AwaitHeadroom);
        assert_eq!(
            without_emits(r.calls()),
            vec![
                Call::Notify("herdr stopped".into()),
                Call::Sample,
                Call::Notify("Waiting for memory".into()),
            ]
        );
    }

    #[test]
    fn panic_in_executor_pauses() {
        let mut r = rig(vec![key("a")], Vec::new(), true);
        r.driver.feed(Event::BootRecovery {
            generation: Generation(1),
            live: Some(inst(5)),
            bypass_gate: false,
            owed: 0,
        });
        assert_eq!(r.phase(), Phase::Paused);
        assert_eq!(r.view.read().unwrap().phase, Phase::Paused);
        assert_eq!(r.cache.read().unwrap().reason_code, Some("internal"));
        // The driver still works afterwards.
        r.driver.feed(Event::UserResumeNow);
        r.settle();
        assert_eq!(r.phase(), Phase::Healthy);
    }

    #[test]
    fn gate_cell_published_before_commands_run() {
        let mut r = rig(vec![key("a"), key("b")], Vec::new(), false);
        r.driver.feed(Event::BootRecovery {
            generation: Generation(1),
            live: Some(inst(5)),
            bypass_gate: false,
            owed: 0,
        });
        r.settle();
        // Each resume ran while the gate already admitted that key.
        assert_eq!(*r.admitted.lock().unwrap(), vec![Ok(()), Ok(())]);
        let view = r.view.read().unwrap();
        assert_eq!(view.phase, Phase::Healthy);
        assert!(view.inflight.is_empty());
        // Both resumed: the published view and payload list them as settled.
        assert_eq!(view.settled, HashSet::from([key("a"), key("b")]));
        let settled: Vec<String> = r
            .cache
            .read()
            .unwrap()
            .settled
            .iter()
            .map(|s| s.worktree.clone())
            .collect();
        assert_eq!(settled, vec!["a", "b"]);
    }

    #[test]
    fn record_timing_goes_to_the_lifecycle_log() {
        let mut r = rig(vec![key("a"), key("b")], Vec::new(), false);
        let ((), events) = lifecycle::with_capture(|| {
            r.driver.feed(Event::BootRecovery {
                generation: Generation(1),
                live: Some(inst(5)),
                bypass_gate: false,
                owed: 0,
            });
            r.settle();
        });
        assert_eq!(r.phase(), Phase::Healthy);
        let timings: Vec<&LifecycleEvent> = events
            .iter()
            .filter(|e| matches!(e, LifecycleEvent::RecoveryTiming { .. }))
            .collect();
        assert_eq!(
            timings,
            vec![&LifecycleEvent::RecoveryTiming {
                sustain_ms: 0,
                resume_ms: 0,
                sessions: 2,
                failed: 0,
                final_cap: 3,
            }]
        );
    }

    #[test]
    fn recv_timeout_uses_next_deadline() {
        let r = rig(Vec::new(), vec![Some(low())], false);
        let base = r.base;
        let now = Arc::clone(&r.now);
        let mut waits = Vec::new();
        let mut script: VecDeque<Event> = VecDeque::from([Event::Death {
            ev: death_of(1),
            generation: Generation(2),
        }]);
        let mut fired = 0;
        r.driver.run_with(|timeout| {
            waits.push(timeout);
            if let Some(ev) = script.pop_front() {
                return Some(ev);
            }
            // Wait out the timer once, then stop.
            if fired == 0 {
                fired += 1;
                let t = timeout.expect("a timer while waiting for memory");
                *now.lock().unwrap() += t;
                return Some(Event::TimerFired);
            }
            None
        });
        // Idle and healthy: no timer, wait for an event.
        assert_eq!(waits[0], None);
        // After the failing sample the next check is 5 s away.
        assert_eq!(waits[1], Some(Duration::from_secs(5)));
        // After that check the next gap is 5 s again, measured from the new now.
        assert_eq!(waits[2], Some(Duration::from_secs(5)));
        assert!(*r.now.lock().unwrap() >= base + Duration::from_secs(5));
    }

    #[test]
    fn notifications_once_per_transition_point_and_none_for_user_actions() {
        // A full episode: death -> memory wait -> restart -> resume -> done.
        let mut r = rig(
            vec![key("a")],
            vec![Some(low()), Some(low()), Some(ample())],
            false,
        );
        r.observe(&[key("a")]);
        r.driver.feed(Event::Death {
            ev: death_of(1),
            generation: Generation(2),
        });
        r.settle();
        assert_eq!(r.phase(), Phase::Healthy);
        assert_eq!(
            r.notifier.titles(),
            vec!["herdr stopped", "Waiting for memory", "Sessions resumed"]
        );

        // Three deaths inside the breaker window: exactly one Paused notification.
        let mut r = rig(Vec::new(), Vec::new(), false);
        for (i, pid) in [1u32, 2, 3].into_iter().enumerate() {
            r.driver.feed(Event::Death {
                ev: death_of(pid),
                generation: Generation(i as u64 + 2),
            });
            r.settle();
        }
        assert_eq!(r.phase(), Phase::Paused);
        let titles = r.notifier.titles();
        assert_eq!(
            titles.iter().filter(|t| *t == "Recovery paused").count(),
            1,
            "{titles:?}"
        );

        // A user restart and a boot recovery notify nothing.
        let mut r = rig(Vec::new(), Vec::new(), false);
        r.driver.feed(Event::UserRestart {
            generation: Generation(2),
        });
        r.settle();
        let mut b = rig(Vec::new(), Vec::new(), false);
        b.driver.feed(Event::BootRecovery {
            generation: Generation(1),
            live: None,
            bypass_gate: false,
            owed: 0,
        });
        b.settle();
        assert_eq!(r.phase(), Phase::Healthy);
        assert_eq!(b.phase(), Phase::Healthy);
        assert!(r.notifier.titles().is_empty());
        assert!(b.notifier.titles().is_empty());
    }

    fn boot_live() -> Event {
        Event::BootRecovery {
            generation: Generation(1),
            live: Some(inst(5)),
            bypass_gate: false,
            owed: 0,
        }
    }

    /// Wait for `n` worker results on the driver's channel.
    fn worker_results(r: &Rig, n: usize) -> Vec<Event> {
        (0..n)
            .map(|_| {
                r.rx.recv_timeout(Duration::from_secs(30))
                    .expect("a worker result")
            })
            .collect()
    }

    fn result_keys(results: &[Event]) -> HashSet<SessionKey> {
        results
            .iter()
            .map(|ev| match ev {
                Event::ResumeResult { key, .. } => key.clone(),
                other => panic!("not a resume result: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn threaded_resumes_send_their_results_from_workers() {
        let mut r = rig_with(
            vec![key("a"), key("b")],
            Vec::new(),
            false,
            ResumeDispatch::Threads,
        );
        r.driver.feed(boot_live());
        // Both resumes went to workers; the machine waits for their answers.
        assert_eq!(r.phase(), Phase::Resuming);
        assert_eq!(r.driver.machine().inflight().len(), 2);
        let results = worker_results(&r, 2);
        assert_eq!(result_keys(&results), HashSet::from([key("a"), key("b")]));
        // Each worker hands back the attempt the machine started it with.
        let attempts: HashSet<u64> = results
            .iter()
            .map(|ev| match ev {
                Event::ResumeResult { attempt, .. } => *attempt,
                other => panic!("not a resume result: {other:?}"),
            })
            .collect();
        assert_eq!(attempts, HashSet::from([1, 2]));
        for ev in &results {
            assert!(
                matches!(
                    ev,
                    Event::ResumeResult {
                        generation: Generation(1),
                        outcome: ResumeOutcomeKind::Resumed,
                        load_related: false,
                        ..
                    }
                ),
                "{ev:?}"
            );
        }
        // Each worker ran while the gate admitted its key.
        assert_eq!(*r.admitted.lock().unwrap(), vec![Ok(()), Ok(())]);
        for ev in results {
            r.driver.feed(ev);
        }
        r.settle();
        assert_eq!(r.phase(), Phase::Healthy);
        assert_eq!(
            r.view.read().unwrap().settled,
            HashSet::from([key("a"), key("b")])
        );
    }

    #[test]
    fn a_worker_whose_generation_moved_on_sends_nothing() {
        let r = rig_with(
            vec![key("a"), key("b")],
            Vec::new(),
            false,
            ResumeDispatch::Threads,
        );
        *r.runner.behaviour.lock().unwrap() = Behaviour::Skip;
        let Rig {
            mut driver,
            rx,
            calls,
            ..
        } = r;
        driver.feed(boot_live());
        // Once the driver and every worker are gone the channel disconnects,
        // so this collects everything any worker sent.
        drop(driver);
        let sent: Vec<Event> = rx.iter().collect();
        assert!(sent.is_empty(), "{sent:?}");
        let resumes = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, Call::Resume(..)))
            .count();
        assert_eq!(resumes, 2);
    }

    #[test]
    fn a_panicking_worker_reports_failed() {
        let r = rig_with(vec![key("a")], Vec::new(), false, ResumeDispatch::Threads);
        *r.runner.behaviour.lock().unwrap() = Behaviour::Panic;
        let mut r = r;
        r.driver.feed(boot_live());
        let results = worker_results(&r, 1);
        assert_eq!(
            results,
            vec![Event::ResumeResult {
                key: key("a"),
                generation: Generation(1),
                attempt: 1,
                outcome: ResumeOutcomeKind::Failed("resume panicked".into()),
                load_related: false,
            }]
        );
        // The machine takes the failure like any other.
        r.driver.feed(results.into_iter().next().unwrap());
        assert!(r.driver.machine().inflight().is_empty());
    }

    #[test]
    fn an_inline_panicking_runner_fails_the_resume_without_pausing() {
        let mut r = rig(vec![key("a")], Vec::new(), false);
        *r.runner.behaviour.lock().unwrap() = Behaviour::Panic;
        r.driver.feed(boot_live());
        assert_ne!(r.phase(), Phase::Paused);
        assert_eq!(r.driver.machine().snapshot().failed, 1);
    }

    #[test]
    fn shutdown_stops_the_driver() {
        // `run_with`: nothing after the shutdown is processed.
        let r = rig(vec![key("a")], Vec::new(), false);
        let mut script = VecDeque::from([Event::Shutdown, boot_live()]);
        let mut asked = 0;
        r.driver.run_with(|_| {
            asked += 1;
            script.pop_front()
        });
        assert_eq!(asked, 1);
        assert_eq!(script.len(), 1);

        // `run`: the driver holds a sender of its own channel, so only the
        // shutdown ends it.
        let Rig { driver, rx, .. } = rig(Vec::new(), Vec::new(), false);
        let tx = driver.tx.clone();
        let join = std::thread::spawn(move || driver.run(rx));
        tx.send(Event::Shutdown).unwrap();
        join.join().unwrap();
    }
}

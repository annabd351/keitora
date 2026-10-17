//! `recovery` — bring the herdr server and its sessions back after a death.
//!
//! [`machine`] is the pure policy (backoff, crash-loop breaker, memory gate,
//! bounded concurrent resume); [`payload`] is the `daemon://recovery` wire shape and its
//! user-facing sentences; [`driver`] is the one thread that runs the machine
//! against the world.
//!
//! The rest of the backend talks to it through a [`RecoveryHandle`] (events in,
//! cached payload out) and asks the [`RecoveryGate`] who may use the server.

pub mod driver;
pub mod machine;
pub mod payload;

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::backend::SessionKey;

use super::gate::{GateRefusal, ServerGate};
use super::ops::{Generation, ServerInstance};
use super::watchdog::DeathEvent;
use driver::{Driver, Executor, ResumeDispatch};
use machine::{Event, Observation, RecoveryMachine, RecoveryParams};

pub use machine::Phase;
pub use payload::RecoveryPayload;

/// The recovery phase under the name the rest of the backend and the wire use.
pub type RecoveryPhase = Phase;

/// What the gate needs from the machine: the phase, the keys whose resume the
/// driver is running, and the keys whose resume already succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateView {
    pub phase: Phase,
    pub inflight: HashSet<SessionKey>,
    pub settled: HashSet<SessionKey>,
}

impl Default for GateView {
    fn default() -> Self {
        GateView {
            phase: Phase::Healthy,
            inflight: HashSet::new(),
            settled: HashSet::new(),
        }
    }
}

/// The [`ServerGate`] over the driver's published view.
pub struct RecoveryGate {
    view: Arc<RwLock<GateView>>,
}

impl RecoveryGate {
    pub fn from_view(view: Arc<RwLock<GateView>>) -> Self {
        RecoveryGate { view }
    }

    pub fn phase(&self) -> Phase {
        self.view.read().map(|v| v.phase).unwrap_or(Phase::Healthy)
    }

    /// Put the gate in `phase` with `inflight` and `settled` (tests without a
    /// driver).
    #[cfg(test)]
    pub fn set_view(
        &self,
        phase: Phase,
        inflight: HashSet<SessionKey>,
        settled: HashSet<SessionKey>,
    ) {
        *self.view.write().unwrap() = GateView {
            phase,
            inflight,
            settled,
        };
    }
}

impl ServerGate for RecoveryGate {
    fn admit(&self, key: Option<&SessionKey>) -> Result<(), GateRefusal> {
        let Ok(view) = self.view.read() else {
            return Err(GateRefusal::RecoveryPending);
        };
        match view.phase {
            Phase::Healthy => Ok(()),
            Phase::Paused => Err(GateRefusal::Paused),
            // Only the driver's own resume of a key in flight.
            Phase::Resuming if key.is_some_and(|k| view.inflight.contains(k)) => Ok(()),
            _ => Err(GateRefusal::RecoveryPending),
        }
    }

    fn admit_session(&self, key: &SessionKey) -> Result<(), GateRefusal> {
        let Ok(view) = self.view.read() else {
            return Err(GateRefusal::RecoveryPending);
        };
        match view.phase {
            Phase::Healthy => Ok(()),
            // A pause stops everything, settled sessions included.
            Phase::Paused => Err(GateRefusal::Paused),
            // A session already resumed this episode stays usable while the
            // rest of the queue resumes or waits for memory: the settled set is
            // emptied by every death, boot, restart and stop, so it only ever
            // names sessions live on the current server.
            _ if view.settled.contains(key) => Ok(()),
            // One in flight is the driver's own.
            Phase::Resuming if view.inflight.contains(key) => Ok(()),
            _ => Err(GateRefusal::RecoveryPending),
        }
    }
}

/// Shows an OS notification. Delivery is best-effort; the banner is the fallback.
pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str);
}

/// Shows nothing (headless use and tests that do not look).
pub struct NoopNotifier;

impl Notifier for NoopNotifier {
    fn notify(&self, _title: &str, _body: &str) {}
}

impl Notifier for tauri::AppHandle {
    fn notify(&self, title: &str, body: &str) {
        use tauri_plugin_notification::NotificationExt;
        let _ = self.notification().builder().title(title).body(body).show();
    }
}

/// Reads the payload the recovery driver last published. Unlike a
/// [`RecoveryHandle`] it cannot send the driver anything, so the driver's own
/// executor can hold one without keeping the driver alive.
#[derive(Clone)]
pub struct PayloadReader {
    cache: Arc<RwLock<RecoveryPayload>>,
}

impl PayloadReader {
    /// The payload the driver last published.
    pub fn snapshot(&self) -> RecoveryPayload {
        self.cache
            .read()
            .map(|c| c.clone())
            .unwrap_or_else(|_| RecoveryPayload::healthy())
    }
}

/// Talks to the recovery driver from any thread. Every method is non-blocking;
/// a driver that is gone just drops the event.
#[derive(Clone)]
pub struct RecoveryHandle {
    tx: Sender<Event>,
    cache: Arc<RwLock<RecoveryPayload>>,
}

impl RecoveryHandle {
    fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    /// The server died; `generation` is the value after the backend bumped it.
    pub fn death(&self, ev: DeathEvent, generation: Generation) {
        self.send(Event::Death { ev, generation });
    }

    pub fn boot_recovery(
        &self,
        generation: Generation,
        live: Option<ServerInstance>,
        bypass_gate: bool,
        owed: u32,
    ) {
        self.send(Event::BootRecovery {
            generation,
            live,
            bypass_gate,
            owed,
        });
    }

    pub fn user_restart(&self, generation: Generation) {
        self.send(Event::UserRestart { generation });
    }

    pub fn explicit_stop(&self) {
        self.send(Event::ExplicitStop);
    }

    /// Stop the driver (the backend is going away). Resume workers still
    /// running finish on their own; their results go nowhere.
    pub fn shutdown(&self) {
        self.send(Event::Shutdown);
    }

    pub fn resume_now(&self) {
        self.send(Event::UserResumeNow);
    }

    pub fn focus(&self, key: &SessionKey) {
        self.send(Event::UserFocus(key.clone()));
    }

    /// Ask the driver to start `key` fresh in its own slot. `false` (and nothing
    /// sent) when recovery holds no entry for `key` that is not already being
    /// resumed: the caller then starts it fresh the ordinary way.
    pub fn start_fresh(&self, key: &SessionKey) -> bool {
        let queued = self
            .cache
            .read()
            .map(|c| {
                c.queue.iter().any(|i| {
                    i.project == key.project && i.worktree == key.worktree && i.state != "resuming"
                })
            })
            .unwrap_or(false);
        if queued {
            self.send(Event::UserStartFresh(key.clone()));
        }
        queued
    }

    pub fn cancel(&self, key: &SessionKey) {
        self.send(Event::Cancel(key.clone()));
    }

    pub fn cancel_project(&self, project: &str) {
        self.send(Event::CancelProject(project.to_string()));
    }

    pub fn observe(&self, obs: &[Observation]) {
        self.send(Event::Observed(obs.to_vec()));
    }

    /// Hold `key` out of the resume queue until the guard drops.
    pub fn transfer_lock(&self, key: &SessionKey) -> TransferGuard {
        self.send(Event::TransferLock(key.clone(), true));
        TransferGuard {
            tx: self.tx.clone(),
            key: key.clone(),
        }
    }

    /// The payload the driver last published.
    pub fn snapshot(&self) -> RecoveryPayload {
        self.reader().snapshot()
    }

    /// A reader of the published payload that sends the driver nothing.
    pub fn reader(&self) -> PayloadReader {
        PayloadReader {
            cache: Arc::clone(&self.cache),
        }
    }
}

/// A handle with no driver behind it, and the events it sends (tests that
/// check what a caller tells the controller).
#[cfg(test)]
pub fn recording_handle() -> (RecoveryHandle, Receiver<Event>) {
    let (handle, _gate, parked) = controller(RecoveryParams::default());
    (handle, parked.rx)
}

/// Keeps a session out of the resume queue while a transfer rewrites it;
/// releases it on drop.
pub struct TransferGuard {
    tx: Sender<Event>,
    key: SessionKey,
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Event::TransferLock(self.key.clone(), false));
    }
}

/// The driver's side, built before the backend starts and spawned by `start`.
pub struct Parked {
    machine: RecoveryMachine,
    rx: Receiver<Event>,
    /// The controller's sender, for the driver's resume workers.
    tx: Sender<Event>,
    view: Arc<RwLock<GateView>>,
    cache: Arc<RwLock<RecoveryPayload>>,
}

impl Parked {
    /// Run the driver on its own thread over `exec`, resuming on worker
    /// threads; it ends on [`RecoveryHandle::shutdown`]. Unit tests resume on
    /// the driver thread so their fake herdr call logs stay deterministic (the
    /// driver's own tests cover the worker threads).
    pub fn spawn(self, exec: Box<dyn Executor>) -> std::io::Result<JoinHandle<()>> {
        let dispatch = if cfg!(test) {
            ResumeDispatch::Inline
        } else {
            ResumeDispatch::Threads
        };
        let driver = Driver::new(
            self.machine,
            exec,
            self.view,
            self.cache,
            Box::new(Instant::now),
            self.tx,
            dispatch,
        );
        let rx = self.rx;
        std::thread::Builder::new()
            .name("herdr-recovery".into())
            .spawn(move || driver.run(rx))
    }
}

/// A recovery controller: the handle, the gate over its view, and the driver
/// waiting to be spawned.
pub fn controller(params: RecoveryParams) -> (RecoveryHandle, Arc<RecoveryGate>, Parked) {
    let (tx, rx) = mpsc::channel();
    let view = Arc::new(RwLock::new(GateView::default()));
    let cache = Arc::new(RwLock::new(RecoveryPayload::healthy()));
    let handle = RecoveryHandle {
        tx: tx.clone(),
        cache: Arc::clone(&cache),
    };
    let gate = Arc::new(RecoveryGate::from_view(Arc::clone(&view)));
    let parked = Parked {
        machine: RecoveryMachine::new(params, Instant::now()),
        rx,
        tx,
        view,
        cache,
    };
    (handle, gate, parked)
}

#[cfg(test)]
mod tests {
    use super::payload::QueueItem;
    use super::*;

    fn key(w: &str) -> SessionKey {
        SessionKey {
            project: "p".into(),
            worktree: w.into(),
        }
    }

    fn gate_in(phase: Phase, inflight: &[&str], settled: &[&str]) -> RecoveryGate {
        RecoveryGate::from_view(Arc::new(RwLock::new(GateView {
            phase,
            inflight: inflight.iter().map(|w| key(w)).collect(),
            settled: settled.iter().map(|w| key(w)).collect(),
        })))
    }

    #[test]
    fn gate_admit_maps_phases() {
        let a = key("a");
        let g = gate_in(Phase::Healthy, &[], &[]);
        assert_eq!(g.admit(None), Ok(()));
        assert_eq!(g.admit(Some(&a)), Ok(()));
        let g = gate_in(Phase::Paused, &[], &[]);
        assert_eq!(g.admit(None), Err(GateRefusal::Paused));
        assert_eq!(g.admit(Some(&a)), Err(GateRefusal::Paused));
        for phase in [
            Phase::Down,
            Phase::AwaitHeadroom,
            Phase::Restarting,
            Phase::Reconciling,
            Phase::Resuming,
        ] {
            let g = gate_in(phase, &[], &[]);
            assert_eq!(
                g.admit(None),
                Err(GateRefusal::RecoveryPending),
                "{phase:?}"
            );
            assert_eq!(
                g.admit(Some(&a)),
                Err(GateRefusal::RecoveryPending),
                "{phase:?}"
            );
        }
    }

    #[test]
    fn gate_admit_is_keyed() {
        let (a, b) = (key("a"), key("b"));
        let g = gate_in(Phase::Resuming, &["a"], &[]);
        assert_eq!(g.admit(Some(&a)), Ok(()));
        assert_eq!(g.admit(Some(&b)), Err(GateRefusal::RecoveryPending));
        assert_eq!(g.admit(None), Err(GateRefusal::RecoveryPending));
        for phase in [Phase::AwaitHeadroom, Phase::Reconciling] {
            let g = gate_in(phase, &["a"], &[]);
            assert_eq!(
                g.admit(Some(&a)),
                Err(GateRefusal::RecoveryPending),
                "{phase:?}"
            );
        }
    }

    #[test]
    fn gate_admits_every_key_in_flight_and_refuses_the_rest() {
        let (a, b, c, d) = (key("a"), key("b"), key("c"), key("d"));
        // Several resumes in flight, one already settled.
        let g = gate_in(Phase::Resuming, &["a", "b"], &["c"]);
        assert_eq!(g.admit(Some(&a)), Ok(()));
        assert_eq!(g.admit(Some(&b)), Ok(()));
        // `admit` is the driver's own resume path: a settled key is not in
        // flight, and a queued one neither.
        assert_eq!(g.admit(Some(&c)), Err(GateRefusal::RecoveryPending));
        assert_eq!(g.admit(Some(&d)), Err(GateRefusal::RecoveryPending));
        assert_eq!(g.admit(None), Err(GateRefusal::RecoveryPending));
        // Paused admits nobody, whatever the sets hold.
        let g = gate_in(Phase::Paused, &["a"], &["c"]);
        assert_eq!(g.admit(Some(&a)), Err(GateRefusal::Paused));
        assert_eq!(g.admit(Some(&c)), Err(GateRefusal::Paused));
    }

    #[test]
    fn admit_session_admits_settled_keys_in_every_phase_but_paused() {
        let (a, b, q, f) = (key("a"), key("b"), key("queued"), key("failed"));
        // `a` in flight, `b` settled; `queued` and `failed` are in neither set.
        let g = gate_in(Phase::Resuming, &["a"], &["b"]);
        assert_eq!(g.admit_session(&a), Ok(()));
        assert_eq!(g.admit_session(&b), Ok(()));
        assert_eq!(g.admit_session(&q), Err(GateRefusal::RecoveryPending));
        assert_eq!(g.admit_session(&f), Err(GateRefusal::RecoveryPending));
        // Keyless callers still wait for Healthy.
        assert_eq!(g.admit(None), Err(GateRefusal::RecoveryPending));
        // Paused admits nobody, settled keys included.
        let g = gate_in(Phase::Paused, &["a"], &["b"]);
        assert_eq!(g.admit_session(&a), Err(GateRefusal::Paused));
        assert_eq!(g.admit_session(&b), Err(GateRefusal::Paused));
        // A settled session stays usable when the queue falls back to waiting
        // for memory (or any other non-paused phase); in-flight keys and the
        // rest unlock only in Resuming.
        for phase in [
            Phase::Down,
            Phase::AwaitHeadroom,
            Phase::Restarting,
            Phase::Reconciling,
        ] {
            let g = gate_in(phase, &["a"], &["b"]);
            assert_eq!(g.admit_session(&b), Ok(()), "{phase:?}");
            assert_eq!(
                g.admit_session(&a),
                Err(GateRefusal::RecoveryPending),
                "{phase:?}"
            );
            assert_eq!(
                g.admit_session(&q),
                Err(GateRefusal::RecoveryPending),
                "{phase:?}"
            );
            // The keyed driver path is unaffected by `settled`.
            assert_eq!(
                g.admit(Some(&b)),
                Err(GateRefusal::RecoveryPending),
                "{phase:?}"
            );
        }
        let g = gate_in(Phase::Healthy, &[], &[]);
        assert_eq!(g.admit_session(&q), Ok(()));
    }

    #[test]
    fn set_view_publishes_both_sets() {
        let g = gate_in(Phase::Healthy, &[], &[]);
        g.set_view(
            Phase::Resuming,
            HashSet::from([key("a")]),
            HashSet::from([key("b")]),
        );
        assert_eq!(g.phase(), Phase::Resuming);
        assert_eq!(g.admit(Some(&key("a"))), Ok(()));
        assert_eq!(g.admit(Some(&key("b"))), Err(GateRefusal::RecoveryPending));
    }

    fn handle_with(queue: Vec<QueueItem>) -> (RecoveryHandle, Receiver<Event>) {
        let (handle, _gate, parked) = controller(RecoveryParams::default());
        *handle.cache.write().unwrap() = RecoveryPayload {
            queue,
            ..RecoveryPayload::healthy()
        };
        (handle, parked.rx)
    }

    fn item(w: &str, state: &'static str) -> QueueItem {
        QueueItem {
            project: "p".into(),
            worktree: w.into(),
            state,
        }
    }

    #[test]
    fn start_fresh_only_for_a_waiting_entry() {
        let (h, rx) = handle_with(vec![
            item("inflight", "resuming"),
            item("queued", "queued"),
            item("parked", "suspended"),
        ]);
        assert!(!h.start_fresh(&key("inflight")));
        assert!(!h.start_fresh(&key("unknown")));
        assert!(h.start_fresh(&key("queued")));
        assert!(h.start_fresh(&key("parked")));
        let sent: Vec<Event> = rx.try_iter().collect();
        assert_eq!(
            sent,
            vec![
                Event::UserStartFresh(key("queued")),
                Event::UserStartFresh(key("parked")),
            ]
        );
    }

    #[test]
    fn transfer_guard_locks_then_unlocks_on_drop() {
        let (h, rx) = handle_with(Vec::new());
        let guard = h.transfer_lock(&key("a"));
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![Event::TransferLock(key("a"), true)]
        );
        drop(guard);
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![Event::TransferLock(key("a"), false)]
        );
    }

    #[test]
    fn snapshot_reads_the_cache() {
        let (h, _rx) = handle_with(vec![item("a", "queued")]);
        let snap = h.snapshot();
        assert_eq!(snap.queue, vec![item("a", "queued")]);
        assert_eq!(snap.phase, Phase::Healthy);
    }

    #[test]
    fn handle_methods_send_their_events() {
        let (h, rx) = handle_with(Vec::new());
        h.user_restart(Generation(3));
        h.explicit_stop();
        h.resume_now();
        h.focus(&key("a"));
        h.cancel(&key("b"));
        h.cancel_project("p");
        h.boot_recovery(Generation(1), None, true, 0);
        h.boot_recovery(Generation(2), None, false, 7);
        h.shutdown();
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![
                Event::UserRestart {
                    generation: Generation(3)
                },
                Event::ExplicitStop,
                Event::UserResumeNow,
                Event::UserFocus(key("a")),
                Event::Cancel(key("b")),
                Event::CancelProject("p".into()),
                Event::BootRecovery {
                    generation: Generation(1),
                    live: None,
                    bypass_gate: true,
                    owed: 0
                },
                Event::BootRecovery {
                    generation: Generation(2),
                    live: None,
                    bypass_gate: false,
                    owed: 7
                },
                Event::Shutdown,
            ]
        );
    }

    #[test]
    fn spawned_driver_publishes_through_the_handle_and_gate() {
        let (handle, gate, parked) = controller(RecoveryParams::default());
        struct Idle;
        impl Executor for Idle {
            fn sample_headroom(&mut self) -> Option<crate::headroom::HeadroomSnapshot> {
                None
            }
            fn start_server(
                &mut self,
                _: bool,
                _: crate::lifecycle::ServerStartHow,
            ) -> Result<ServerInstance, String> {
                Err("no server in this test".into())
            }
            fn reconcile(
                &mut self,
                _: Generation,
                _: ServerInstance,
            ) -> Result<super::super::reconcile::ReconcileOutcome, String> {
                Err("no server in this test".into())
            }
            fn resume_runner(&self) -> Arc<dyn driver::ResumeRunner> {
                Arc::new(IdleRunner)
            }
            fn notify(&mut self, _: &str, _: &str) {}
            fn emit(&mut self, _: &RecoveryPayload) {}
        }
        struct IdleRunner;
        impl driver::ResumeRunner for IdleRunner {
            fn resume(
                &self,
                _: &SessionKey,
                _: Generation,
                _: machine::ResumeMode,
            ) -> Option<driver::ResumeReport> {
                Some(driver::ResumeReport {
                    outcome: machine::ResumeOutcomeKind::Failed("no server in this test".into()),
                    load_related: false,
                })
            }
        }
        let join = parked.spawn(Box::new(Idle)).unwrap();
        handle.explicit_stop();
        handle.user_restart(Generation(1));
        // The start fails three times; each retry waits on a backoff, so the
        // phase settles in Restarting or Down, never Healthy, within a moment.
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let left_healthy =
            || handle.snapshot().phase != Phase::Healthy && gate.phase() != Phase::Healthy;
        while !left_healthy() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(left_healthy());
        // The driver holds a sender itself: only the shutdown ends it.
        handle.shutdown();
        join.join().unwrap();
    }
}

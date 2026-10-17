//! `gate` — who may use the herdr server right now.
//!
//! After the server dies, nothing may spawn, attach to or start it except the
//! explicit recovery entry points. Every backend caller asks a [`ServerGate`]
//! first: ordinary callers (`key = None`) are admitted only while recovery is
//! idle, and the recovery driver's own queued resume of a key (`key = Some(k)`)
//! is admitted while that key is among those being resumed. Callers that act on
//! one live session (attach, write, resize, repaint) use
//! [`ServerGate::admit_session`], which also lets through a session whose
//! resume already succeeded this episode, in every phase but a pause.
//!
//! The recovery controller's `RecoveryGate` is the implementation; the
//! [`RecoveryPayload`] (the `daemon://recovery` wire shape) and [`RecoveryPhase`]
//! live in the recovery module and are re-exported here.

use crate::backend::SessionKey;

pub use super::recovery::RecoveryPayload;

/// The event every recovery transition is pushed on.
pub const RECOVERY_EVENT: &str = "daemon://recovery";

/// Why a gate refused a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateRefusal {
    /// The server is down or being recovered.
    RecoveryPending,
    /// Recovery stopped and is waiting for the user.
    Paused,
}

pub trait ServerGate: Send + Sync {
    /// `key = None`: an ordinary caller (spawn, kill, the session listings, the
    /// status loop), admitted only when recovery is idle (`Healthy`).
    /// `key = Some(k)`: the recovery driver launching its own queued resume of
    /// `k`, admitted when `Healthy`, or while `Resuming` with `k` in flight.
    fn admit(&self, key: Option<&SessionKey>) -> Result<(), GateRefusal>;

    /// A caller acting on the live session `key` (attach, write, resize,
    /// repaint). The recovery gate also admits a session whose resume already
    /// succeeded this episode in every phase but `Paused`; by default this is
    /// the keyed [`Self::admit`].
    fn admit_session(&self, key: &SessionKey) -> Result<(), GateRefusal> {
        self.admit(Some(key))
    }
}

#[cfg(test)]
mod tests {
    use super::super::recovery::RecoveryPhase;
    use super::*;
    use std::sync::Mutex;

    /// A gate that records every `admit` and answers from a script.
    struct ScriptedGate {
        seen: Mutex<Vec<Option<SessionKey>>>,
        answer: Result<(), GateRefusal>,
    }

    impl ServerGate for ScriptedGate {
        fn admit(&self, key: Option<&SessionKey>) -> Result<(), GateRefusal> {
            self.seen.lock().unwrap().push(key.cloned());
            self.answer
        }
    }

    #[test]
    fn admit_session_defaults_to_the_keyed_admit() {
        let key = SessionKey {
            project: "p".into(),
            worktree: "a".into(),
        };
        for answer in [
            Ok(()),
            Err(GateRefusal::RecoveryPending),
            Err(GateRefusal::Paused),
        ] {
            let g = ScriptedGate {
                seen: Mutex::new(Vec::new()),
                answer,
            };
            assert_eq!(g.admit_session(&key), answer);
            assert_eq!(*g.seen.lock().unwrap(), vec![Some(key.clone())]);
        }
    }

    #[test]
    fn phase_names() {
        let all = [
            (RecoveryPhase::Healthy, "healthy"),
            (RecoveryPhase::Down, "down"),
            (RecoveryPhase::AwaitHeadroom, "await_headroom"),
            (RecoveryPhase::Restarting, "restarting"),
            (RecoveryPhase::Reconciling, "reconciling"),
            (RecoveryPhase::Resuming, "resuming"),
            (RecoveryPhase::Paused, "paused"),
        ];
        for (p, name) in all {
            assert_eq!(p.as_str(), name);
            let wire = serde_json::to_value(p).unwrap();
            assert_eq!(wire, serde_json::Value::String(format!("{p:?}")));
        }
    }

    #[test]
    fn payload_json_has_every_field_with_nulls() {
        let v = serde_json::to_value(RecoveryPayload::healthy()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "phase": "Healthy",
                "reason": null,
                "reason_code": null,
                "queue": [],
                "settled": [],
                "resumed": 0,
                "failed": 0,
                "total": 0,
                "generation": 0,
                "can_resume_now": false,
                "seq": 0,
            })
        );
    }
}

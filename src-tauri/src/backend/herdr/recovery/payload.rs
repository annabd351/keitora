//! `daemon://recovery` wire shape and the user-facing reason sentences.
//!
//! The payload always serializes every field (the frontend reads `null`, never a
//! missing key). `seq` increments on every snapshot; it is deliberately not named
//! `revision`, so the frontend's live-resource revision gate never skips an event.

use std::time::Duration;

use serde::Serialize;

use super::machine::{DeathClass, PauseReason, Phase, RecoveryParams};

/// One session in the recovery queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueueItem {
    pub project: String,
    pub worktree: String,
    /// `"queued"` | `"resuming"` | `"suspended"`.
    pub state: &'static str,
}

/// A session by name (the settled list on the wire).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionRef {
    pub project: String,
    pub worktree: String,
}

/// The `daemon://recovery` event body (and the `daemon_recovery` command's
/// answer). Every field is always serialized; "none" is `null`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecoveryPayload {
    pub phase: Phase,
    /// User-facing sentence; `null` when healthy with nothing left over.
    pub reason: Option<String>,
    /// Machine-readable cause; `null` when there is none.
    pub reason_code: Option<&'static str>,
    /// Sessions still owed a resume, in priority order (including suspended
    /// leftovers while healthy).
    pub queue: Vec<QueueItem>,
    /// Sessions whose resume succeeded in this episode and may be used while
    /// the rest still resume, sorted by `(project, worktree)`.
    pub settled: Vec<SessionRef>,
    pub resumed: u32,
    pub failed: u32,
    pub total: u32,
    pub generation: u64,
    /// Whether the user can trigger the recovery now.
    pub can_resume_now: bool,
    /// Increments on every snapshot, so no event is ever mistaken for a repeat.
    pub seq: u64,
}

impl RecoveryPayload {
    /// Nothing to recover.
    pub fn healthy() -> Self {
        RecoveryPayload {
            phase: Phase::Healthy,
            reason: None,
            reason_code: None,
            queue: Vec::new(),
            settled: Vec::new(),
            resumed: 0,
            failed: 0,
            total: 0,
            generation: 0,
            can_resume_now: false,
            seq: 0,
        }
    }
}

/// What the reason sentence describes: the phase plus the facts its text needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Situation<'a> {
    /// Healthy; `suspended` leftovers wait for the user or the healthy reset.
    Healthy {
        suspended: usize,
    },
    /// Down, waiting out `backoff` before checking memory.
    Down {
        backoff: Duration,
    },
    AwaitHeadroom {
        server_up: bool,
    },
    Restarting,
    Reconciling,
    Resuming {
        done: u32,
        total: u32,
    },
    Paused(&'a PauseReason),
}

/// `"1 minute"` / `"5 minutes"`.
pub fn count(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `"1 session"` / `"3 sessions"`.
pub fn sessions(n: u64) -> String {
    count(n, "session", "sessions")
}

/// Whole minutes when the duration is a multiple of a minute, else seconds.
fn about(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        count(secs / 60, "minute", "minutes")
    } else {
        count(secs, "second", "seconds")
    }
}

/// The sentence for a paused recovery (also the body of the paused notification).
pub fn paused_text(reason: &PauseReason, p: &RecoveryParams) -> String {
    let n = u64::from(p.breaker_deaths);
    let m = count(p.breaker_window.as_secs() / 60, "minute", "minutes");
    match reason {
        PauseReason::CrashLoop(DeathClass::MemoryPressure) => format!(
            "herdr stopped {} in {m} while the system was low on memory.",
            count(n, "time", "times")
        ),
        PauseReason::CrashLoop(DeathClass::EarlyDeath) => format!(
            "herdr crashed {} in {m} right after starting.",
            count(n, "time", "times")
        ),
        PauseReason::CrashLoop(DeathClass::Unknown) => format!(
            "herdr stopped unexpectedly {} in {m}.",
            count(n, "time", "times")
        ),
        PauseReason::ResumeRetrip(_) => format!(
            "herdr stopped again within {} of resuming.",
            count(p.resume_retrip.as_secs() / 60, "minute", "minutes")
        ),
        PauseReason::HeadroomCap => format!(
            "Not enough free memory after {}. Resume now ignores the memory check.",
            count(p.headroom_cap.as_secs() / 60, "minute", "minutes")
        ),
        PauseReason::StartFailed => "herdr could not be started.".to_string(),
        PauseReason::ReconcileFailed => {
            "Could not restore sessions after herdr restarted.".to_string()
        }
        PauseReason::Internal => "Recovery stopped unexpectedly. See keitora.log.".to_string(),
    }
}

/// The user-facing reason for a situation; `None` when healthy with nothing left.
pub fn reason_text(situation: Situation<'_>, p: &RecoveryParams) -> Option<String> {
    Some(match situation {
        Situation::Healthy { suspended: 0 } => return None,
        Situation::Healthy { suspended } => format!(
            "{} suspended. Click one to resume it, or resume them all.",
            sessions(suspended as u64)
        ),
        Situation::Down { backoff } if backoff.is_zero() => "herdr stopped.".to_string(),
        Situation::Down { backoff } => {
            format!("herdr stopped. Restarting in about {}.", about(backoff))
        }
        Situation::AwaitHeadroom { server_up: false } => {
            "Waiting for free memory before restarting herdr.".to_string()
        }
        Situation::AwaitHeadroom { server_up: true } => {
            "Waiting for free memory before resuming the remaining sessions.".to_string()
        }
        Situation::Restarting => "Restarting herdr\u{2026}".to_string(),
        Situation::Reconciling => "Restoring sessions\u{2026}".to_string(),
        Situation::Resuming { done, total } => {
            format!("Resuming sessions ({done} of {total})\u{2026}")
        }
        Situation::Paused(reason) => paused_text(reason, p),
    })
}

/// The machine-readable code for a situation; `None` when there is none.
pub fn reason_code(situation: Situation<'_>) -> Option<&'static str> {
    match situation {
        Situation::Healthy { suspended } if suspended > 0 => Some("suspended"),
        Situation::Paused(reason) => Some(match reason {
            PauseReason::CrashLoop(DeathClass::MemoryPressure) => "crash_loop_memory",
            PauseReason::CrashLoop(DeathClass::EarlyDeath) => "crash_loop_early",
            PauseReason::CrashLoop(DeathClass::Unknown) => "crash_loop_unknown",
            PauseReason::ResumeRetrip(_) => "resume_retrip",
            PauseReason::HeadroomCap => "headroom_cap",
            PauseReason::StartFailed => "start_failed",
            PauseReason::ReconcileFailed => "reconcile_failed",
            PauseReason::Internal => "internal",
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_reason_text_matches_class() {
        let p = RecoveryParams::default();
        let rows: [(PauseReason, &str, &str); 9] = [
            (
                PauseReason::CrashLoop(DeathClass::MemoryPressure),
                "herdr stopped 3 times in 30 minutes while the system was low on memory.",
                "crash_loop_memory",
            ),
            (
                PauseReason::CrashLoop(DeathClass::EarlyDeath),
                "herdr crashed 3 times in 30 minutes right after starting.",
                "crash_loop_early",
            ),
            (
                PauseReason::CrashLoop(DeathClass::Unknown),
                "herdr stopped unexpectedly 3 times in 30 minutes.",
                "crash_loop_unknown",
            ),
            (
                PauseReason::ResumeRetrip(DeathClass::Unknown),
                "herdr stopped again within 10 minutes of resuming.",
                "resume_retrip",
            ),
            (
                PauseReason::ResumeRetrip(DeathClass::MemoryPressure),
                "herdr stopped again within 10 minutes of resuming.",
                "resume_retrip",
            ),
            (
                PauseReason::HeadroomCap,
                "Not enough free memory after 10 minutes. Resume now ignores the memory check.",
                "headroom_cap",
            ),
            (
                PauseReason::StartFailed,
                "herdr could not be started.",
                "start_failed",
            ),
            (
                PauseReason::ReconcileFailed,
                "Could not restore sessions after herdr restarted.",
                "reconcile_failed",
            ),
            (
                PauseReason::Internal,
                "Recovery stopped unexpectedly. See keitora.log.",
                "internal",
            ),
        ];
        for (reason, text, code) in rows {
            let s = Situation::Paused(&reason);
            assert_eq!(reason_text(s, &p).as_deref(), Some(text), "{reason:?}");
            assert_eq!(reason_code(s), Some(code), "{reason:?}");
        }
    }

    #[test]
    fn paused_text_singular_and_custom_params() {
        let p = RecoveryParams {
            breaker_deaths: 1,
            breaker_window: Duration::from_secs(60),
            resume_retrip: Duration::from_secs(60),
            headroom_cap: Duration::from_secs(60),
            ..RecoveryParams::default()
        };
        assert_eq!(
            paused_text(&PauseReason::CrashLoop(DeathClass::Unknown), &p),
            "herdr stopped unexpectedly 1 time in 1 minute."
        );
        assert_eq!(
            paused_text(&PauseReason::ResumeRetrip(DeathClass::Unknown), &p),
            "herdr stopped again within 1 minute of resuming."
        );
        assert_eq!(
            paused_text(&PauseReason::HeadroomCap, &p),
            "Not enough free memory after 1 minute. Resume now ignores the memory check."
        );
    }

    #[test]
    fn reason_text_for_every_phase() {
        let p = RecoveryParams::default();
        let cases: Vec<(Situation<'_>, Option<&str>, Option<&str>)> = vec![
            (Situation::Healthy { suspended: 0 }, None, None),
            (
                Situation::Healthy { suspended: 1 },
                Some("1 session suspended. Click one to resume it, or resume them all."),
                Some("suspended"),
            ),
            (
                Situation::Healthy { suspended: 4 },
                Some("4 sessions suspended. Click one to resume it, or resume them all."),
                Some("suspended"),
            ),
            (
                Situation::Down {
                    backoff: Duration::from_secs(60),
                },
                Some("herdr stopped. Restarting in about 1 minute."),
                None,
            ),
            (
                Situation::Down {
                    backoff: Duration::from_secs(300),
                },
                Some("herdr stopped. Restarting in about 5 minutes."),
                None,
            ),
            (
                Situation::Down {
                    backoff: Duration::ZERO,
                },
                Some("herdr stopped."),
                None,
            ),
            (
                Situation::AwaitHeadroom { server_up: false },
                Some("Waiting for free memory before restarting herdr."),
                None,
            ),
            (
                Situation::AwaitHeadroom { server_up: true },
                Some("Waiting for free memory before resuming the remaining sessions."),
                None,
            ),
            (
                Situation::Restarting,
                Some("Restarting herdr\u{2026}"),
                None,
            ),
            (
                Situation::Reconciling,
                Some("Restoring sessions\u{2026}"),
                None,
            ),
            (
                Situation::Resuming { done: 2, total: 5 },
                Some("Resuming sessions (2 of 5)\u{2026}"),
                None,
            ),
        ];
        for (s, text, code) in cases {
            assert_eq!(reason_text(s, &p).as_deref(), text, "{s:?}");
            assert_eq!(reason_code(s), code, "{s:?}");
        }
    }

    #[test]
    fn healthy_payload_serializes_every_field() {
        let v = serde_json::to_value(RecoveryPayload::healthy()).unwrap();
        let obj = v.as_object().unwrap();
        for key in [
            "phase",
            "reason",
            "reason_code",
            "queue",
            "settled",
            "resumed",
            "failed",
            "total",
            "generation",
            "can_resume_now",
            "seq",
        ] {
            assert!(obj.contains_key(key), "missing {key}");
        }
        assert_eq!(obj.len(), 11);
        assert_eq!(v["phase"], "Healthy");
        assert!(v["reason"].is_null());
        assert!(v["reason_code"].is_null());
    }
}

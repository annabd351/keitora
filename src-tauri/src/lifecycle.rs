//! `lifecycle` — structured records of server starts/deaths, recovery phase
//! changes and per-session resume outcomes.
//!
//! [`record`] writes one `key=value` line on the [`LIFECYCLE_TARGET`] log target,
//! which the app logger always routes to `keitora.log` regardless of `RUST_LOG`.
//! It is fire-and-forget: it never fails, blocks or panics the caller. Tests use
//! [`with_capture`] to assert what was recorded without touching the filesystem.
//!
//! The types here are deliberately flat (std types + [`HeadroomSnapshot`]) so this
//! module depends on nothing the watchdog / recovery / resume code defines; those
//! convert their own events into these records.

use crate::backend::SessionKey;
use crate::headroom::{format_fields, HeadroomSnapshot};
use crate::logging::LIFECYCLE_TARGET;

/// How a server instance came to be running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStartHow {
    // Every start now goes through recovery and is labelled by its reason; a
    // plain spawn remains for callers outside it (tests).
    #[allow(dead_code)]
    Spawned,
    Adopted,
    Restart,
    BootRecovery,
}

impl ServerStartHow {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServerStartHow::Spawned => "spawned",
            ServerStartHow::Adopted => "adopted",
            ServerStartHow::Restart => "restart",
            ServerStartHow::BootRecovery => "boot_recovery",
        }
    }
}

/// A server death, as the watchdog observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDeathRecord {
    pub pid: Option<u32>,
    /// The child's exit code; `None` when unknown (adopted server, or death by signal).
    pub exit_code: Option<i32>,
    /// Unix only: the terminating signal.
    pub signal: Option<i32>,
    /// `"child"` | `"adopted"`.
    pub source: &'static str,
    pub uptime_secs: Option<u64>,
    pub headroom: Option<HeadroomSnapshot>,
}

/// How a session's agent came back after a server restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeResult {
    Resumed { id: String },
    Continued,
    Fresh,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    ServerStart {
        pid: Option<u32>,
        how: ServerStartHow,
        headroom: Option<HeadroomSnapshot>,
    },
    ServerDeath(ServerDeathRecord),
    /// Phase names are the recovery machine's snake_case phase names.
    RecoveryPhase {
        from: &'static str,
        to: &'static str,
        reason: String,
    },
    ResumeOutcome {
        key: SessionKey,
        outcome: ResumeResult,
    },
    /// How long a recovery that resumed sessions took, once it is healthy.
    RecoveryTiming {
        sustain_ms: u64,
        resume_ms: u64,
        sessions: u32,
        failed: u32,
        final_cap: u32,
    },
}

/// Quote a value when it contains a space, `"` or `=`, or is empty; `"` and `\`
/// are backslash-escaped inside the quotes.
fn q(v: &str) -> String {
    if v.is_empty() || v.contains([' ', '"', '=']) {
        let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        v.to_string()
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "unknown".to_string(), |v| v.to_string())
}

/// The record's line body (no timestamp / level): space-separated `key=value`
/// pairs, always starting with `event=`.
pub fn format_event(e: &LifecycleEvent) -> String {
    match e {
        LifecycleEvent::ServerStart { pid, how, headroom } => format!(
            "event=server_start pid={} how={} {}",
            opt(*pid),
            how.as_str(),
            format_fields(headroom.as_ref())
        ),
        LifecycleEvent::ServerDeath(d) => {
            let mut out = format!(
                "event=server_death source={} pid={} exit_code={}",
                q(d.source),
                opt(d.pid),
                opt(d.exit_code)
            );
            if let Some(code) = d.exit_code {
                out.push_str(&format!(" exit_hex={:#010x}", code as u32));
            }
            if let Some(sig) = d.signal {
                out.push_str(&format!(" signal={sig}"));
            }
            out.push_str(&format!(
                " uptime_s={} {}",
                opt(d.uptime_secs),
                format_fields(d.headroom.as_ref())
            ));
            out
        }
        LifecycleEvent::RecoveryPhase { from, to, reason } => format!(
            "event=recovery_phase from={} to={} reason={}",
            q(from),
            q(to),
            q(reason)
        ),
        LifecycleEvent::ResumeOutcome { key, outcome } => {
            let key = q(&key.to_string());
            let outcome = match outcome {
                ResumeResult::Resumed { id } => format!("resumed id={}", q(id)),
                ResumeResult::Continued => "continued".to_string(),
                ResumeResult::Fresh => "fresh".to_string(),
                ResumeResult::Failed { reason } => format!("failed reason={}", q(reason)),
            };
            format!("event=resume_outcome key={key} outcome={outcome}")
        }
        LifecycleEvent::RecoveryTiming {
            sustain_ms,
            resume_ms,
            sessions,
            failed,
            final_cap,
        } => format!(
            "event=recovery_timing sustain_ms={sustain_ms} resume_ms={resume_ms} sessions={sessions} failed={failed} final_cap={final_cap}"
        ),
    }
}

/// Record a lifecycle event. Never fails and never panics.
pub fn record(e: LifecycleEvent) {
    log::info!(target: LIFECYCLE_TARGET, "{}", format_event(&e));
    #[cfg(test)]
    capture::push(e);
}

#[cfg(test)]
mod capture {
    use std::cell::RefCell;

    use super::LifecycleEvent;

    thread_local! {
        pub(super) static CAPTURE: RefCell<Option<Vec<LifecycleEvent>>> =
            const { RefCell::new(None) };
    }

    pub(super) fn push(e: LifecycleEvent) {
        let _ = CAPTURE.try_with(|c| {
            if let Ok(mut c) = c.try_borrow_mut() {
                if let Some(v) = c.as_mut() {
                    v.push(e);
                }
            }
        });
    }
}

/// Run `f` with capture armed on this thread; returns `f`'s result and every
/// event [`record`]ed on this thread meanwhile, in order.
#[cfg(test)]
pub fn with_capture<R>(f: impl FnOnce() -> R) -> (R, Vec<LifecycleEvent>) {
    let prev = capture::CAPTURE.with(|c| c.replace(Some(Vec::new())));
    let r = f();
    let got = capture::CAPTURE
        .with(|c| c.replace(prev))
        .unwrap_or_default();
    (r, got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headroom::{MemoryPressure, MIB};

    fn snap() -> HeadroomSnapshot {
        HeadroomSnapshot {
            commit_avail: 5120 * MIB,
            phys_avail: 3200 * MIB,
            pressure: None,
        }
    }

    fn death() -> ServerDeathRecord {
        ServerDeathRecord {
            pid: Some(1234),
            exit_code: Some(-1073740791),
            signal: None,
            source: "child",
            uptime_secs: Some(3600),
            headroom: Some(HeadroomSnapshot {
                commit_avail: 412 * MIB,
                phys_avail: 900 * MIB,
                pressure: None,
            }),
        }
    }

    fn key() -> SessionKey {
        SessionKey {
            project: "project".into(),
            worktree: "worktree".into(),
        }
    }

    #[test]
    fn server_start_known_pid_with_headroom() {
        let e = LifecycleEvent::ServerStart {
            pid: Some(1234),
            how: ServerStartHow::Spawned,
            headroom: Some(snap()),
        };
        assert_eq!(
            format_event(&e),
            "event=server_start pid=1234 how=spawned commit_avail_mib=5120 phys_avail_mib=3200"
        );
    }

    #[test]
    fn server_start_unknown_pid_and_headroom() {
        let e = LifecycleEvent::ServerStart {
            pid: None,
            how: ServerStartHow::Adopted,
            headroom: None,
        };
        assert_eq!(
            format_event(&e),
            "event=server_start pid=unknown how=adopted headroom=unknown"
        );
    }

    #[test]
    fn server_start_with_pressure_and_other_hows() {
        let e = LifecycleEvent::ServerStart {
            pid: Some(1),
            how: ServerStartHow::Restart,
            headroom: Some(HeadroomSnapshot {
                pressure: Some(MemoryPressure::Warn),
                ..snap()
            }),
        };
        assert_eq!(
            format_event(&e),
            "event=server_start pid=1 how=restart commit_avail_mib=5120 phys_avail_mib=3200 pressure=warn"
        );
        let e = LifecycleEvent::ServerStart {
            pid: Some(1),
            how: ServerStartHow::BootRecovery,
            headroom: None,
        };
        assert_eq!(
            format_event(&e),
            "event=server_start pid=1 how=boot_recovery headroom=unknown"
        );
    }

    #[test]
    fn server_death_negative_windows_exit_code() {
        assert_eq!(
            format_event(&LifecycleEvent::ServerDeath(death())),
            "event=server_death source=child pid=1234 exit_code=-1073740791 exit_hex=0xc0000409 \
             uptime_s=3600 commit_avail_mib=412 phys_avail_mib=900"
        );
    }

    #[test]
    fn server_death_all_unknown() {
        let d = ServerDeathRecord {
            pid: None,
            exit_code: None,
            signal: None,
            source: "adopted",
            uptime_secs: None,
            headroom: None,
        };
        assert_eq!(
            format_event(&LifecycleEvent::ServerDeath(d)),
            "event=server_death source=adopted pid=unknown exit_code=unknown uptime_s=unknown \
             headroom=unknown"
        );
    }

    #[test]
    fn server_death_with_signal() {
        let d = ServerDeathRecord {
            exit_code: None,
            signal: Some(9),
            headroom: None,
            ..death()
        };
        assert_eq!(
            format_event(&LifecycleEvent::ServerDeath(d)),
            "event=server_death source=child pid=1234 exit_code=unknown signal=9 uptime_s=3600 \
             headroom=unknown"
        );
        let d = ServerDeathRecord {
            exit_code: Some(1),
            signal: None,
            ..death()
        };
        assert!(format_event(&LifecycleEvent::ServerDeath(d))
            .contains("exit_code=1 exit_hex=0x00000001 uptime_s=3600"));
    }

    #[test]
    fn recovery_phase_quotes_reason() {
        let e = LifecycleEvent::RecoveryPhase {
            from: "down",
            to: "await_headroom",
            reason: "commit 412 MiB < required 4696 MiB".into(),
        };
        assert_eq!(
            format_event(&e),
            "event=recovery_phase from=down to=await_headroom reason=\"commit 412 MiB < required 4696 MiB\""
        );
        let e = LifecycleEvent::RecoveryPhase {
            from: "healthy",
            to: "down",
            reason: r#"a "b" c\d"#.into(),
        };
        assert_eq!(
            format_event(&e),
            r#"event=recovery_phase from=healthy to=down reason="a \"b\" c\\d""#
        );
        let e = LifecycleEvent::RecoveryPhase {
            from: "a",
            to: "b",
            reason: String::new(),
        };
        assert_eq!(
            format_event(&e),
            r#"event=recovery_phase from=a to=b reason="""#
        );
    }

    #[test]
    fn recovery_timing_plain_integers() {
        let e = LifecycleEvent::RecoveryTiming {
            sustain_ms: 0,
            resume_ms: 41_250,
            sessions: 24,
            failed: 3,
            final_cap: 1,
        };
        assert_eq!(
            format_event(&e),
            "event=recovery_timing sustain_ms=0 resume_ms=41250 sessions=24 failed=3 final_cap=1"
        );
    }

    #[test]
    fn resume_outcome_variants() {
        let fmt = |outcome| {
            format_event(&LifecycleEvent::ResumeOutcome {
                key: key(),
                outcome,
            })
        };
        assert_eq!(
            fmt(ResumeResult::Resumed {
                id: "abc123".into()
            }),
            "event=resume_outcome key=project/worktree outcome=resumed id=abc123"
        );
        assert_eq!(
            fmt(ResumeResult::Continued),
            "event=resume_outcome key=project/worktree outcome=continued"
        );
        assert_eq!(
            fmt(ResumeResult::Fresh),
            "event=resume_outcome key=project/worktree outcome=fresh"
        );
        assert_eq!(
            fmt(ResumeResult::Failed {
                reason: "no transcript".into()
            }),
            "event=resume_outcome key=project/worktree outcome=failed reason=\"no transcript\""
        );
    }

    #[test]
    fn resume_outcome_quotes_key_with_space() {
        let e = LifecycleEvent::ResumeOutcome {
            key: SessionKey {
                project: "my project".into(),
                worktree: "main".into(),
            },
            outcome: ResumeResult::Fresh,
        };
        assert_eq!(
            format_event(&e),
            "event=resume_outcome key=\"my project/main\" outcome=fresh"
        );
    }

    #[test]
    fn with_capture_returns_recorded_events_in_order() {
        let a = LifecycleEvent::ServerStart {
            pid: Some(1),
            how: ServerStartHow::Spawned,
            headroom: None,
        };
        let b = LifecycleEvent::ResumeOutcome {
            key: key(),
            outcome: ResumeResult::Continued,
        };
        let (r, got) = with_capture(|| {
            record(a.clone());
            record(b.clone());
            7
        });
        assert_eq!(r, 7);
        assert_eq!(got, vec![a, b]);
        // Capture is disarmed afterwards.
        let ((), after) = with_capture(|| {});
        assert!(after.is_empty());
    }

    #[test]
    fn record_without_capture_does_not_panic() {
        record(LifecycleEvent::ServerStart {
            pid: None,
            how: ServerStartHow::Adopted,
            headroom: None,
        });
    }
}

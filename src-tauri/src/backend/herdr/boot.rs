//! `boot` — what to do about herdr when Keitora launches.
//!
//! The boot probe asks the server whether it is running (retrying only on a
//! timeout or a resource-pressure failure); [`classify_boot`] then compares the
//! answer with the server instance remembered in the keymap:
//!
//! | live | remembered | sessions | class |
//! |---|---|---|---|
//! | could not tell | any | any | `Indeterminate` |
//! | absent | none | any | `Fresh` |
//! | absent | some | no | `DownNoSessions` |
//! | absent | some | yes | `RecoverDown` |
//! | present `i` | `i` | any | `Adopt` |
//! | present | none (or either identity is unknown) | any | `Adopt` |
//! | present `i` | another | any | `RecoverDifferentInstance` |
//!
//! The first matching row wins. A missing remembered instance with a live
//! server is the upgrade case: it is adopted, never recovered. An identity whose
//! start time could not be read (0) is matched by pid alone.

use std::time::Duration;

use super::ops::{HerdrError, HerdrOps, ServerInstance};
use super::watchdog::UNKNOWN_INSTANCE;

/// Boot probe attempts before the server's state counts as unknown.
pub const BOOT_PROBE_ATTEMPTS: u32 = 3;
/// Delay between boot probe attempts.
pub const BOOT_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// What the boot probe found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveServer {
    /// Nothing is running.
    Absent,
    /// A server answers; [`UNKNOWN_INSTANCE`] when its identity could not be read.
    Present(ServerInstance),
    /// Every attempt failed: the server's state is unknown.
    Indeterminate,
}

/// What launching Keitora does about herdr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootClass {
    /// Nothing ran before: start the server.
    Fresh,
    /// The server Keitora last used is still up: keep using it.
    Adopt,
    /// The server is down but nothing is owed a resume: start it.
    DownNoSessions,
    /// The server died while Keitora was closed: start it and resume sessions.
    RecoverDown,
    /// A different server is up (it restored the panes as shells): reconcile
    /// and resume without starting anything.
    RecoverDifferentInstance,
    /// Could not reach herdr: do nothing until the user asks.
    Indeterminate,
}

/// Classify the boot situation (pure).
pub fn classify_boot(
    persisted: Option<&ServerInstance>,
    live: &LiveServer,
    has_sessions: bool,
) -> BootClass {
    match (live, persisted) {
        (LiveServer::Indeterminate, _) => BootClass::Indeterminate,
        (LiveServer::Absent, None) => BootClass::Fresh,
        (LiveServer::Absent, Some(_)) if !has_sessions => BootClass::DownNoSessions,
        (LiveServer::Absent, Some(_)) => BootClass::RecoverDown,
        (LiveServer::Present(_), None) => BootClass::Adopt,
        // An identity that is unknown or only a pid is compared as far as it
        // goes; when nothing can be told, the live server is adopted.
        (LiveServer::Present(i), Some(p)) => match p.same_process(i) {
            Some(false) => BootClass::RecoverDifferentInstance,
            Some(true) | None => BootClass::Adopt,
        },
    }
}

/// Whether a failed boot probe is worth another try.
fn retryable(e: &HerdrError) -> bool {
    matches!(e, HerdrError::Timeout) || e.is_resource_pressure()
}

/// Ask the server whether it runs, up to [`BOOT_PROBE_ATTEMPTS`] times
/// `interval` apart, retrying only a timeout or a resource-pressure failure.
/// Returns the finding and, when indeterminate, the last error (typed, so the
/// caller can tell a missing herdr executable from an unreachable server).
pub fn boot_probe(
    ops: &dyn HerdrOps,
    interval: Duration,
    sleep: &dyn Fn(Duration),
) -> (LiveServer, Option<HerdrError>) {
    let mut last = None;
    for attempt in 0..BOOT_PROBE_ATTEMPTS {
        match ops.status_server() {
            Ok(s) if s.running => {
                let instance = ops.server_instance().unwrap_or(UNKNOWN_INSTANCE);
                return (LiveServer::Present(instance), None);
            }
            Ok(_) => return (LiveServer::Absent, None),
            Err(e) => {
                log::warn!(
                    "herdr: boot status check {} of {BOOT_PROBE_ATTEMPTS} failed: {e}",
                    attempt + 1
                );
                let again = retryable(&e);
                last = Some(e);
                if !again {
                    break;
                }
            }
        }
        if attempt + 1 < BOOT_PROBE_ATTEMPTS {
            sleep(interval);
        }
    }
    (LiveServer::Indeterminate, last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::fake::FakeHerdr;
    use crate::backend::herdr::ops::Op;
    use std::sync::Mutex;

    const A: ServerInstance = ServerInstance {
        pid: 10,
        start_time: 100,
    };
    const B: ServerInstance = ServerInstance {
        pid: 11,
        start_time: 200,
    };

    #[test]
    fn indeterminate_wins_regardless_of_the_rest() {
        for persisted in [None, Some(&A)] {
            for has in [false, true] {
                assert_eq!(
                    classify_boot(persisted, &LiveServer::Indeterminate, has),
                    BootClass::Indeterminate
                );
            }
        }
    }

    #[test]
    fn absent_server_rows() {
        for has in [false, true] {
            assert_eq!(
                classify_boot(None, &LiveServer::Absent, has),
                BootClass::Fresh
            );
        }
        assert_eq!(
            classify_boot(Some(&A), &LiveServer::Absent, false),
            BootClass::DownNoSessions
        );
        assert_eq!(
            classify_boot(Some(&A), &LiveServer::Absent, true),
            BootClass::RecoverDown
        );
    }

    #[test]
    fn present_server_rows() {
        for has in [false, true] {
            assert_eq!(
                classify_boot(Some(&A), &LiveServer::Present(A), has),
                BootClass::Adopt,
                "the same server"
            );
            assert_eq!(
                classify_boot(None, &LiveServer::Present(A), has),
                BootClass::Adopt,
                "nothing remembered: the upgrade case adopts"
            );
            assert_eq!(
                classify_boot(Some(&A), &LiveServer::Present(UNKNOWN_INSTANCE), has),
                BootClass::Adopt,
                "the live server cannot be identified"
            );
            assert_eq!(
                classify_boot(Some(&A), &LiveServer::Present(B), has),
                BootClass::RecoverDifferentInstance
            );
        }
    }

    #[test]
    fn an_identity_without_a_start_time_is_matched_by_pid() {
        let pid_only = |pid| ServerInstance { pid, start_time: 0 };
        for has in [false, true] {
            assert_eq!(
                classify_boot(Some(&pid_only(10)), &LiveServer::Present(A), has),
                BootClass::Adopt,
                "remembered without a start time: the same process"
            );
            assert_eq!(
                classify_boot(Some(&A), &LiveServer::Present(pid_only(10)), has),
                BootClass::Adopt,
                "live without a start time: the same process"
            );
            assert_eq!(
                classify_boot(Some(&pid_only(10)), &LiveServer::Present(B), has),
                BootClass::RecoverDifferentInstance,
                "another pid is another process"
            );
            assert_eq!(
                classify_boot(Some(&UNKNOWN_INSTANCE), &LiveServer::Present(A), has),
                BootClass::Adopt,
                "nothing known about the remembered server"
            );
        }
    }

    fn probe(fake: &FakeHerdr) -> (LiveServer, Option<HerdrError>, Vec<Duration>) {
        let slept = Mutex::new(Vec::new());
        let (live, err) = boot_probe(fake, BOOT_PROBE_INTERVAL, &|d| {
            slept.lock().unwrap().push(d)
        });
        (live, err, slept.into_inner().unwrap())
    }

    fn status_calls(fake: &FakeHerdr) -> usize {
        fake.calls()
            .iter()
            .filter(|c| c.op == Op::StatusServer)
            .count()
    }

    #[test]
    fn probe_retries_timeouts_until_the_server_answers() {
        let fake = FakeHerdr::new();
        fake.fail_next(Op::StatusServer, HerdrError::Timeout);
        fake.fail_next(Op::StatusServer, HerdrError::Timeout);
        let (live, err, slept) = probe(&fake);
        assert_eq!(live, LiveServer::Present(fake.server_instance().unwrap()));
        assert_eq!(err, None);
        assert_eq!(slept, vec![BOOT_PROBE_INTERVAL; 2]);
        assert_eq!(status_calls(&fake), 3);
    }

    #[test]
    fn probe_under_memory_pressure_is_indeterminate_after_three_tries() {
        let fake = FakeHerdr::new();
        for _ in 0..3 {
            fake.fail_next(
                Op::StatusServer,
                HerdrError::SpawnFailed {
                    // Commit limit reached (an out-of-memory errno elsewhere).
                    os_error: Some(if cfg!(windows) { 1455 } else { 12 }),
                    message: "paging file too small".into(),
                },
            );
        }
        let (live, err, slept) = probe(&fake);
        assert_eq!(live, LiveServer::Indeterminate);
        assert!(err.is_some());
        assert_eq!(slept.len(), 2);
        assert_eq!(status_calls(&fake), 3);
    }

    #[test]
    fn probe_does_not_retry_other_failures() {
        let fake = FakeHerdr::new();
        fake.fail_next(
            Op::StatusServer,
            HerdrError::Parse {
                message: "garbage".into(),
            },
        );
        let (live, err, slept) = probe(&fake);
        assert_eq!(live, LiveServer::Indeterminate);
        assert!(err.is_some_and(|e| !e.is_exe_missing()));
        assert!(slept.is_empty());
        assert_eq!(status_calls(&fake), 1);
    }

    #[test]
    fn probe_with_a_missing_executable_is_indeterminate_at_once_with_the_typed_error() {
        let fake = FakeHerdr::new();
        fake.set_exe_missing(true);
        let (live, err, slept) = probe(&fake);
        assert_eq!(live, LiveServer::Indeterminate);
        assert!(err.is_some_and(|e| e.is_exe_missing()));
        assert!(slept.is_empty(), "a missing file is not worth a retry");
        assert_eq!(status_calls(&fake), 1);
    }

    #[test]
    fn probe_reports_absent_and_unknown_identity() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        assert_eq!(probe(&fake).0, LiveServer::Absent);

        let fake = FakeHerdr::new();
        fake.set_instance(None);
        assert_eq!(probe(&fake).0, LiveServer::Present(UNKNOWN_INSTANCE));
    }
}

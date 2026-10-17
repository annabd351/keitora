//! `exec` — run one short-lived `herdr` CLI call under a deadline.
//!
//! `Command::output()` has no timeout, and `herdr status server` against a local
//! socket never times out on its own, so a wedged or starved herdr could hang a
//! Keitora thread forever. [`run_with_deadline`] polls the child with `try_wait`,
//! kills and reaps it when the deadline passes, and returns
//! [`HerdrError::Timeout`]. No extra dependency: the 10 ms poll is negligible at
//! these call rates.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::ops::HerdrError;

/// Poll interval while waiting for a CLI child to exit.
const POLL_STEP: Duration = Duration::from_millis(10);
/// How long to wait for a pipe reader after the child was killed (a grandchild
/// still holding the pipe must not hang the caller).
const READER_GRACE_AFTER_KILL: Duration = Duration::from_secs(1);
/// How long to wait for a pipe reader after a normal exit.
const READER_GRACE_AFTER_EXIT: Duration = Duration::from_secs(2);

/// The kind of herdr CLI call, which sets its deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallClass {
    /// `status server`.
    Status,
    /// Pane and agent queries/inputs (`pane list|close|run|send-*|read`, `agent list|prompt`).
    Quick,
    /// `workspace create`.
    Create,
    /// `agent start` (herdr itself waits up to the agent-start timeout).
    AgentStart,
    /// `server stop`.
    Stop,
    /// `integration status`.
    IntegrationQuery,
    /// `integration install`.
    IntegrationInstall,
}

impl CallClass {
    pub fn deadline(self) -> Duration {
        match self {
            CallClass::Status => Duration::from_secs(3),
            CallClass::Quick => Duration::from_secs(5),
            CallClass::Create => Duration::from_secs(10),
            CallClass::AgentStart => {
                Duration::from_millis(super::AGENT_START_TIMEOUT_MS) + Duration::from_secs(5)
            }
            CallClass::Stop => Duration::from_secs(10),
            CallClass::IntegrationQuery => Duration::from_secs(10),
            CallClass::IntegrationInstall => Duration::from_secs(30),
        }
    }
}

/// Call `done` every `step` until it returns `true` (-> `true`) or `deadline` has
/// elapsed since the first call to `now` (-> `false`). The clock and sleep are
/// injected so the timing logic is testable without real time.
pub(crate) fn poll_until(
    deadline: Duration,
    step: Duration,
    now: impl Fn() -> Instant,
    sleep: impl Fn(Duration),
    mut done: impl FnMut() -> bool,
) -> bool {
    let end = now() + deadline;
    loop {
        if done() {
            return true;
        }
        if now() >= end {
            return false;
        }
        sleep(step);
    }
}

fn spawn_failed(e: &std::io::Error) -> HerdrError {
    HerdrError::SpawnFailed {
        os_error: e.raw_os_error(),
        message: e.to_string(),
    }
}

/// Read a pipe to the end on its own thread and send the bytes back.
fn read_on_thread(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    } else {
        let _ = tx.send(Vec::new());
    }
    rx
}

/// Run `cmd` (stdin null, stdout/stderr captured) and return its output, or
/// [`HerdrError::Timeout`] after killing it if it is still running at `deadline`.
/// A spawn or `try_wait` I/O error is [`HerdrError::SpawnFailed`].
pub fn run_with_deadline(cmd: Command, deadline: Duration) -> Result<Output, HerdrError> {
    run_with_deadline_observed(cmd, deadline, |_| {})
}

/// [`run_with_deadline`], reporting the child's pid once it is spawned (tests use
/// it to confirm the child is gone after a timeout).
fn run_with_deadline_observed(
    mut cmd: Command,
    deadline: Duration,
    on_spawn: impl FnOnce(u32),
) -> Result<Output, HerdrError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| spawn_failed(&e))?;
    on_spawn(child.id());

    // Drain both pipes concurrently so a chatty child never blocks on a full pipe.
    let stdout_rx = read_on_thread(child.stdout.take());
    let stderr_rx = read_on_thread(child.stderr.take());

    let mut outcome: Option<Result<std::process::ExitStatus, std::io::Error>> = None;
    let completed = poll_until(
        deadline,
        POLL_STEP,
        Instant::now,
        std::thread::sleep,
        || match child.try_wait() {
            Ok(Some(status)) => {
                outcome = Some(Ok(status));
                true
            }
            Ok(None) => false,
            Err(e) => {
                outcome = Some(Err(e));
                true
            }
        },
    );

    let collect =
        |rx: &mpsc::Receiver<Vec<u8>>, grace: Duration| rx.recv_timeout(grace).unwrap_or_default();

    match (completed, outcome) {
        (true, Some(Ok(status))) => Ok(Output {
            status,
            stdout: collect(&stdout_rx, READER_GRACE_AFTER_EXIT),
            stderr: collect(&stderr_rx, READER_GRACE_AFTER_EXIT),
        }),
        (true, Some(Err(e))) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(spawn_failed(&e))
        }
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            // Give the readers a moment to see EOF; a detached reader is harmless.
            let _ = collect(&stdout_rx, READER_GRACE_AFTER_KILL);
            let _ = collect(&stderr_rx, READER_GRACE_AFTER_KILL);
            Err(HerdrError::Timeout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Env var that turns the ignored helper tests below into real child
    /// processes; set only when this test binary re-invokes itself.
    const SLEEPER_ENV: &str = "KEITORA_EXEC_SLEEPER";
    const ECHO_ENV: &str = "KEITORA_EXEC_ECHO";
    const SLEEPER_TEST: &str = "backend::herdr::exec::tests::sleeper";
    const ECHO_TEST: &str = "backend::herdr::exec::tests::echoer";

    /// A child for the kill test: sleeps 30 s when re-invoked with the env set.
    #[test]
    #[ignore = "helper child process for run_with_deadline tests"]
    fn sleeper() {
        if std::env::var_os(SLEEPER_ENV).is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    /// A child for the capture test: writes a marker to stdout and stderr.
    #[test]
    #[ignore = "helper child process for run_with_deadline tests"]
    fn echoer() {
        if std::env::var_os(ECHO_ENV).is_some() {
            println!("exec-stdout-marker");
            eprintln!("exec-stderr-marker");
        }
    }

    /// This test binary, re-invoked to run exactly one ignored helper test.
    fn self_command(test: &str, env: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(env, "1");
        cmd
    }

    #[test]
    fn poll_until_completes_before_deadline() {
        let t0 = Instant::now();
        let clock = Cell::new(t0);
        let mut calls = 0;
        let done = poll_until(
            Duration::from_secs(1),
            Duration::from_millis(10),
            || clock.get(),
            |d| clock.set(clock.get() + d),
            || {
                calls += 1;
                calls == 3
            },
        );
        assert!(done);
        assert_eq!(calls, 3);
        assert_eq!(clock.get() - t0, Duration::from_millis(20));
    }

    #[test]
    fn poll_until_expires_on_fake_clock_with_bounded_steps() {
        let t0 = Instant::now();
        let clock = Cell::new(t0);
        let mut calls = 0u32;
        let done = poll_until(
            Duration::from_millis(100),
            Duration::from_millis(10),
            || clock.get(),
            |d| clock.set(clock.get() + d),
            || {
                calls += 1;
                false
            },
        );
        assert!(!done);
        // One check per step plus the one at the deadline.
        assert_eq!(calls, 11);
        assert_eq!(clock.get() - t0, Duration::from_millis(100));
    }

    #[test]
    fn poll_until_checks_done_once_even_with_zero_deadline() {
        let clock = Cell::new(Instant::now());
        let mut calls = 0;
        assert!(poll_until(
            Duration::ZERO,
            Duration::from_millis(10),
            || clock.get(),
            |d| clock.set(clock.get() + d),
            || {
                calls += 1;
                true
            },
        ));
        assert_eq!(calls, 1);
    }

    #[test]
    fn run_with_deadline_kills_a_child_that_overruns() {
        let pid = Cell::new(None);
        let started = Instant::now();
        let r = run_with_deadline_observed(
            self_command(SLEEPER_TEST, SLEEPER_ENV),
            Duration::from_millis(300),
            |p| pid.set(Some(p)),
        );
        let elapsed = started.elapsed();
        assert_eq!(r.unwrap_err(), HerdrError::Timeout);
        assert!(
            elapsed < Duration::from_secs(5),
            "timed-out call took {elapsed:?}"
        );
        let pid = pid.get().expect("child was spawned");
        assert!(
            !crate::backend::platform::process_is_alive(pid),
            "child {pid} still alive after timeout"
        );
    }

    #[test]
    fn run_with_deadline_returns_captured_output_of_a_fast_child() {
        let out = run_with_deadline(self_command(ECHO_TEST, ECHO_ENV), Duration::from_secs(30))
            .expect("fast child completes");
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stdout.contains("exec-stdout-marker"), "stdout: {stdout}");
        assert!(stderr.contains("exec-stderr-marker"), "stderr: {stderr}");
    }

    #[test]
    fn run_with_deadline_maps_a_missing_binary_to_spawn_failed() {
        let cmd = Command::new("keitora-exec-test-no-such-binary-6f1c");
        match run_with_deadline(cmd, Duration::from_secs(1)) {
            // The exact code depends on how the OS reports a missing program; it is
            // never resource pressure.
            Err(e @ HerdrError::SpawnFailed { .. }) => assert!(!e.is_resource_pressure()),
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
    }

    #[test]
    fn call_class_deadlines() {
        assert_eq!(CallClass::Status.deadline(), Duration::from_secs(3));
        assert_eq!(CallClass::Quick.deadline(), Duration::from_secs(5));
        assert_eq!(CallClass::Create.deadline(), Duration::from_secs(10));
        assert_eq!(CallClass::AgentStart.deadline(), Duration::from_secs(15));
        assert_eq!(CallClass::Stop.deadline(), Duration::from_secs(10));
        assert_eq!(
            CallClass::IntegrationQuery.deadline(),
            Duration::from_secs(10)
        );
        assert_eq!(
            CallClass::IntegrationInstall.deadline(),
            Duration::from_secs(30)
        );
    }
}

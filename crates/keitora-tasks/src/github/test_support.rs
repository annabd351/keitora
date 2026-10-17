//! Test doubles for the GitHub transport seam, shared across this stage's tests
//! and the (future) `GitHubBackend` contract tests.
//!
//! Gated on `#[cfg(test)]` so they never ship in a release build.

use async_trait::async_trait;
use std::sync::Mutex;
use std::time::Duration;

use crate::backend::{BackendError, BackendResult};
use crate::github::retry::Clock;
use crate::github::transport::{GhRequest, GhTransport};

/// A scripted [`GhTransport`]: each call pops the next outcome off a queue, so
/// fault-injection tests can say "first call → `Err(Timeout)`, then `Ok(...)`".
///
/// Also records every [`GhRequest`] it received for assertion.
pub(crate) struct FakeTransport {
    script: Mutex<std::collections::VecDeque<BackendResult<Vec<u8>>>>,
    /// The default outcome returned once the script is exhausted. When `None`,
    /// running past the end of the script panics (catches under-scripted tests).
    default: Option<Vec<u8>>,
    calls: Mutex<Vec<GhRequest>>,
}

impl FakeTransport {
    /// Build a transport from an ordered script of per-call outcomes.
    pub(crate) fn scripted(outcomes: impl IntoIterator<Item = BackendResult<Vec<u8>>>) -> Self {
        Self {
            script: Mutex::new(outcomes.into_iter().collect()),
            default: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A transport that always returns the same canned stdout.
    pub(crate) fn always_ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            script: Mutex::new(std::collections::VecDeque::new()),
            default: Some(body.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Number of times [`GhTransport::run`] was invoked.
    pub(crate) fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// Snapshot of the requests seen so far.
    pub(crate) fn requests(&self) -> Vec<GhRequest> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl GhTransport for FakeTransport {
    async fn run(&self, req: GhRequest) -> BackendResult<Vec<u8>> {
        self.calls.lock().unwrap().push(req);
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(outcome) => outcome,
            None => match &self.default {
                Some(body) => Ok(body.clone()),
                None => Err(BackendError::Permanent(
                    "FakeTransport script exhausted (no default set)".to_string(),
                )),
            },
        }
    }
}

/// A [`Clock`] that records every requested sleep and returns immediately, so
/// backoff / `Retry-After` behavior is asserted with no wall-clock cost.
pub(crate) struct TestClock {
    sleeps: Mutex<Vec<Duration>>,
}

impl TestClock {
    pub(crate) fn new() -> Self {
        Self {
            sleeps: Mutex::new(Vec::new()),
        }
    }

    /// The durations passed to [`Clock::sleep`], in order.
    pub(crate) fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap().clone()
    }
}

#[async_trait]
impl Clock for TestClock {
    async fn sleep(&self, d: Duration) {
        self.sleeps.lock().unwrap().push(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_transport_replays_script() {
        let t = FakeTransport::scripted([Err(BackendError::Timeout), Ok(b"ok".to_vec())]);
        let req = || GhRequest {
            args: vec!["api".into()],
            stdin: None,
            mutation: false,
        };
        assert!(matches!(t.run(req()).await, Err(BackendError::Timeout)));
        assert_eq!(t.run(req()).await.unwrap(), b"ok");
        assert_eq!(t.call_count(), 2);
        assert_eq!(t.requests().len(), 2);
    }

    #[tokio::test]
    async fn fake_transport_always_ok() {
        let t = FakeTransport::always_ok(b"hi".to_vec());
        let req = GhRequest {
            args: vec![],
            stdin: None,
            mutation: true,
        };
        assert_eq!(t.run(req).await.unwrap(), b"hi");
    }

    #[tokio::test]
    async fn test_clock_records_sleeps() {
        let c = TestClock::new();
        c.sleep(Duration::from_secs(1)).await;
        c.sleep(Duration::from_secs(2)).await;
        assert_eq!(
            c.sleeps(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }
}

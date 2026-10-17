//! Bounded, backoff-aware retry helper with an injectable clock.
//!
//! `GitHubBackend` (a later stage) wraps each logical `GhTransport::run` in
//! [`retry`]. Retrying is gated strictly on [`BackendError::is_retryable`], so a
//! `Conflict` / `Unauthorized` / `Config` / `Unavailable` / `NotFound` is returned
//! immediately. The [`Clock`] seam lets tests assert backoff and `Retry-After`
//! honoring without real wall-clock waits.

use async_trait::async_trait;
use std::future::Future;
use std::time::Duration;

use crate::backend::BackendResult;

/// Backoff configuration for [`retry`].
#[derive(Debug, Clone)]
pub(crate) struct RetryPolicy {
    /// Maximum number of attempts (the initial try counts as attempt 1).
    pub max_attempts: u32,
    /// Base backoff; doubled each attempt.
    pub base: Duration,
    /// Cap on the computed exponential backoff.
    pub max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base: Duration::from_millis(200),
            max: Duration::from_secs(10),
        }
    }
}

/// Injectable sleep, so tests run with zero wall-clock cost.
#[async_trait]
pub(crate) trait Clock: Send + Sync {
    async fn sleep(&self, d: Duration);
}

/// Real clock backed by `tokio::time::sleep`.
pub(crate) struct TokioClock;

#[async_trait]
impl Clock for TokioClock {
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// Run `op`, retrying retryable failures up to `policy.max_attempts` times.
///
/// * Retries **only** when [`BackendError::is_retryable`] is true (`RateLimited` /
///   `Timeout` / `Transient`). `Conflict` / `Unauthorized` / `Config` /
///   `Unavailable` / `NotFound` (and any other non-retryable variant) return
///   immediately.
/// * Backoff is exponential from `policy.base`, capped at `policy.max`. If the
///   error carries [`BackendError::retry_after`], the wait is at least that long.
/// * After exhausting attempts, the last error is returned.
pub(crate) async fn retry<T, F, Fut>(
    policy: &RetryPolicy,
    clock: &dyn Clock,
    mut op: F,
) -> BackendResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = BackendResult<T>>,
{
    let max_attempts = policy.max_attempts.max(1);
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                // Non-retryable, or out of attempts → surface the error now.
                if !e.is_retryable() || attempt >= max_attempts {
                    return Err(e);
                }

                // Exponential backoff capped at `max`, with `attempt` starting at 1.
                let exp = backoff(policy.base, policy.max, attempt);
                // Honor a server-provided hint as a floor.
                let wait = match e.retry_after() {
                    Some(hint) => exp.max(hint),
                    None => exp,
                };
                clock.sleep(wait).await;
            }
        }
    }
}

/// `base * 2^(attempt-1)` capped at `max`. `attempt` is 1-based.
fn backoff(base: Duration, max: Duration, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    base.checked_mul(factor as u32).unwrap_or(max).min(max)
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendError;
    use crate::github::test_support::TestClock;
    use std::cell::Cell;

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base: Duration::from_millis(10),
            max: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let clock = TestClock::new();
        let calls = Cell::new(0u32);
        let policy = fast_policy();

        let result: BackendResult<u32> = retry(&policy, &clock, || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n < 3 {
                    Err(BackendError::Transient("boom".into()))
                } else {
                    Ok(n)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 3);
        assert_eq!(calls.get(), 3);
        // Two failed attempts → two recorded sleeps.
        assert_eq!(clock.sleeps().len(), 2);
    }

    #[tokio::test]
    async fn retries_timeout_then_succeeds() {
        let clock = TestClock::new();
        let calls = Cell::new(0u32);
        let policy = fast_policy();

        let result: BackendResult<&str> = retry(&policy, &clock, || {
            let first = calls.get() == 0;
            calls.set(calls.get() + 1);
            async move {
                if first {
                    Err(BackendError::Timeout)
                } else {
                    Ok("ok")
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn conflict_is_not_retried() {
        let clock = TestClock::new();
        let calls = Cell::new(0u32);
        let policy = fast_policy();

        let result: BackendResult<()> = retry(&policy, &clock, || {
            calls.set(calls.get() + 1);
            async { Err(BackendError::Conflict("race".into())) }
        })
        .await;

        assert!(matches!(result, Err(BackendError::Conflict(_))));
        assert_eq!(calls.get(), 1);
        assert!(clock.sleeps().is_empty());
    }

    #[tokio::test]
    async fn unauthorized_is_not_retried() {
        let clock = TestClock::new();
        let calls = Cell::new(0u32);
        let policy = fast_policy();

        let result: BackendResult<()> = retry(&policy, &clock, || {
            calls.set(calls.get() + 1);
            async {
                Err(BackendError::Unauthorized {
                    message: "nope".into(),
                    remediation: None,
                })
            }
        })
        .await;

        assert!(matches!(result, Err(BackendError::Unauthorized { .. })));
        assert_eq!(calls.get(), 1);
        assert!(clock.sleeps().is_empty());
    }

    #[tokio::test]
    async fn rate_limited_retry_after_is_honored_as_floor() {
        let clock = TestClock::new();
        let calls = Cell::new(0u32);
        // Tiny base so the exponential backoff is far below the hint.
        let policy = RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(1),
            max: Duration::from_secs(60),
        };
        let hint = Duration::from_secs(30);

        let result: BackendResult<u32> = retry(&policy, &clock, || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n < 2 {
                    Err(BackendError::RateLimited {
                        retry_after: Some(hint),
                    })
                } else {
                    Ok(n)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 2);
        let sleeps = clock.sleeps();
        assert_eq!(sleeps.len(), 1);
        assert!(
            sleeps[0] >= hint,
            "slept {:?}, expected >= {hint:?}",
            sleeps[0]
        );
    }

    #[tokio::test]
    async fn max_attempts_respected_returns_last_error() {
        let clock = TestClock::new();
        let calls = Cell::new(0u32);
        let policy = RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(1),
            max: Duration::from_millis(5),
        };

        let result: BackendResult<()> = retry(&policy, &clock, || {
            let n = calls.get() + 1;
            calls.set(n);
            async move { Err(BackendError::Transient(format!("fail-{n}"))) }
        })
        .await;

        match result {
            Err(BackendError::Transient(msg)) => assert_eq!(msg, "fail-3"),
            other => panic!("expected last Transient error, got {other:?}"),
        }
        // 3 attempts total, 2 sleeps between them.
        assert_eq!(calls.get(), 3);
        assert_eq!(clock.sleeps().len(), 2);
    }
}

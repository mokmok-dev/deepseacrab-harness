//! Provider retry guidance for transient failures.

use dsh_core::LlmError;
use std::time::Duration;

/// Retry limits and exponential backoff bounds for one provider route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts permitted, including the first one.
    pub max_attempts: u32,
    /// Delay before the second attempt.
    pub base_delay: Duration,
    /// Ceiling applied to every computed delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(3, Duration::from_millis(500), Duration::from_secs(30))
    }
}

impl RetryPolicy {
    /// Build a policy from attempt and delay bounds.
    #[must_use]
    pub const fn new(max_attempts: u32, base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_attempts,
            base_delay,
            max_delay,
        }
    }

    /// Whether another attempt may follow `attempts_made` failed attempts.
    #[must_use]
    pub const fn should_retry(&self, attempts_made: u32, error: &LlmError) -> bool {
        attempts_made < self.max_attempts && error.is_retryable()
    }

    /// Exponential delay after `attempts_made` failed attempts, capped at
    /// `max_delay`.
    #[must_use]
    pub fn delay(&self, attempts_made: u32) -> Duration {
        let shift = attempts_made.saturating_sub(1).min(63);
        let factor = 2_u128.saturating_pow(shift);
        let millis = self.base_delay.as_millis().saturating_mul(factor);
        Duration::from_millis(
            u64::try_from(millis.min(self.max_delay.as_millis())).unwrap_or(u64::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::LlmErrorCode;
    use proptest::prelude::*;

    fn retryable() -> LlmError {
        LlmError::new(LlmErrorCode::Server, "detail", true)
    }

    fn terminal() -> LlmError {
        LlmError::new(LlmErrorCode::InvalidRequest, "detail", false)
    }

    proptest! {
        #[test]
        fn delay_never_exceeds_the_ceiling(
            max_attempts in 0_u32..8,
            base_ms in 0_u64..1_000,
            max_ms in 0_u64..10_000,
            attempts in 0_u32..16,
        ) {
            let policy = RetryPolicy::new(
                max_attempts,
                Duration::from_millis(base_ms),
                Duration::from_millis(max_ms),
            );
            prop_assert!(policy.delay(attempts) <= Duration::from_millis(max_ms));
        }

        #[test]
        fn delay_is_monotonic(
            base_ms in 0_u64..1_000,
            max_ms in 0_u64..10_000,
            attempts in 0_u32..16,
        ) {
            let policy = RetryPolicy::new(5, Duration::from_millis(base_ms), Duration::from_millis(max_ms));
            prop_assert!(policy.delay(attempts + 1) >= policy.delay(attempts));
        }

        #[test]
        fn attempts_are_bounded_by_the_policy(
            max_attempts in 0_u32..8,
            attempts in 0_u32..16,
        ) {
            let policy = RetryPolicy::new(max_attempts, Duration::from_millis(10), Duration::from_secs(1));
            prop_assert_eq!(policy.should_retry(attempts, &retryable()), attempts < max_attempts);
            prop_assert!(!policy.should_retry(attempts, &terminal()));
        }
    }

    #[test]
    fn delay_doubles_until_the_ceiling() {
        let policy = RetryPolicy::new(6, Duration::from_millis(100), Duration::from_millis(450));
        assert_eq!(policy.delay(0), Duration::from_millis(100));
        assert_eq!(policy.delay(1), Duration::from_millis(100));
        assert_eq!(policy.delay(2), Duration::from_millis(200));
        assert_eq!(policy.delay(3), Duration::from_millis(400));
        assert_eq!(policy.delay(4), Duration::from_millis(450));
        assert_eq!(policy.delay(100), Duration::from_millis(450));
    }

    #[test]
    fn default_policy_is_normal_and_retryable() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_attempts, 3);
        assert_eq!(policy.base_delay, Duration::from_millis(500));
        assert_eq!(policy.max_delay, Duration::from_secs(30));
        assert!(policy.should_retry(0, &retryable()));
        assert!(policy.should_retry(2, &retryable()));
        assert!(!policy.should_retry(3, &retryable()));
        assert_eq!(policy.delay(3), Duration::from_secs(2));
    }

    #[test]
    fn zero_attempts_never_retries() {
        let policy = RetryPolicy::new(0, Duration::from_millis(1), Duration::from_millis(1));
        assert!(!policy.should_retry(0, &retryable()));
    }
}

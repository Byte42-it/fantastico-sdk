//! How long to wait before retrying a failure that clears by itself (rate limits, an overloaded
//! provider).
//!
//! Retries wrap a **single HTTP request**: repeating it costs one request. Repeating a whole
//! conversation turn instead would pay again for the work already done.

use std::time::Duration;

use crate::provider_error::ProviderError;

/// How the client retries a rate-limited or overloaded request.
///
/// Only failures where the provider certainly did not generate (and charge for) an answer are
/// retried: `429` and `503`. A `502` or `504` is never retried, because the answer may have been
/// generated and lost on the way back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryConfig {
    /// Retries after the first attempt; `0` turns retrying off.
    pub attempts: u32,
    /// The first wait when the provider doesn't say how long to wait; it doubles on each retry.
    pub base_delay: Duration,
    /// The longest single wait.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    /// Two retries, starting at half a second, never waiting more than ten seconds at once.
    fn default() -> Self {
        Self {
            attempts: 2,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
        }
    }
}

/// The wait before retry number `attempt` (counting from 1).
///
/// The provider's `Retry-After` wins when present; otherwise the wait grows exponentially. Both
/// are capped at `max_delay`, then jittered over the upper half of the window (50% to 100%), so
/// clients that were refused together don't retry together, and the wait never drops below half
/// of what the provider asked for. `random` is in `[0, 1)`.
pub(crate) fn delay_for(
    err: &ProviderError,
    attempt: u32,
    cfg: &RetryConfig,
    random: f64,
) -> Duration {
    let base = u64::try_from(cfg.base_delay.as_millis()).unwrap_or(u64::MAX);
    let wanted = err
        .retry_after_ms
        .unwrap_or_else(|| base.saturating_mul(1u64 << attempt.saturating_sub(1).min(20)));
    let capped = wanted.min(u64::try_from(cfg.max_delay.as_millis()).unwrap_or(u64::MAX));
    let factor = 0.5 + random.clamp(0.0, 1.0) * 0.5;
    Duration::from_millis((capped as f64 * factor).round() as u64)
}

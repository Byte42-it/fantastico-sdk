//! How many output tokens a request can ask for with the runtime key in hand.
//!
//! The provider compares the worst-case cost of a request (`max_tokens` times the output price)
//! with the key's spending cap before generating anything. Without `max_tokens` the worst case
//! is the model's maximum output, so a key minted on a small cap would be refused even when the
//! real answer costs a fraction of a cent. An explicit `max_tokens` from the caller is never
//! changed: this only fills the gap when nobody chose a value.

/// The share of the cap a single request may plan to spend.
const HEADROOM: f64 = 0.8;
/// Above this many tokens the cap is roomy enough to leave `max_tokens` unset.
const COMFORTABLE: f64 = 8192.0;
/// The smallest `max_tokens` worth sending.
const FLOOR: u64 = 64;

/// The `max_tokens` to send, or `None` to leave it unset: without a cap or a price there is
/// nothing to compute, and a guess would be worse than silence.
pub(crate) fn affordable(
    key_limit_usd: Option<f64>,
    price_per_output_token: Option<f64>,
) -> Option<u64> {
    let limit = key_limit_usd.filter(|l| l.is_finite() && *l > 0.0)?;
    let price = price_per_output_token.filter(|p| p.is_finite() && *p > 0.0)?;
    let n = ((limit * HEADROOM) / price).floor();
    if !n.is_finite() || n >= COMFORTABLE {
        return None;
    }
    // `n` is finite and in [0, 8192): the conversion is exact.
    Some((n.max(0.0) as u64).max(FLOOR))
}

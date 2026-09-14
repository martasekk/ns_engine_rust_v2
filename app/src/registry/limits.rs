//! Every knob the registry has, and the one piece of arithmetic it does.
//!
//! Separate from the registry because the numbers are the deployment's and
//! the backoff is a pure function of them: a test can ask what the third
//! failure waits without building anything.

use std::time::Duration;

/// The knobs, in one struct so `new` does not take seven scalars.
#[derive(Debug, Clone)]
pub(crate) struct RegistryLimits {
    /// How long a company may be silent before its engine is torn down.
    ///
    /// Its own knob, and far longer than the session idle timeout on
    /// purpose: a session going quiet costs a summary, a company going
    /// quiet costs a rebuild - providers re-dialled, a store re-opened,
    /// `learned.toml` re-read. Minutes, not seconds.
    pub idle_evict_after: Duration,
    /// How often the serve loop looks for idle companies.
    pub sweep_every: Duration,
    /// First backoff after a failure; doubles per consecutive failure.
    pub backoff_base: Duration,
    /// The ceiling that doubling stops at.
    pub backoff_max: Duration,
    /// How many *distinct* companies must report a store error inside
    /// `store_window` before the shard is considered gone (B5).
    pub store_fatal_tenants: usize,
    pub store_window: Duration,
}

impl Default for RegistryLimits {
    fn default() -> Self {
        Self {
            idle_evict_after: Duration::from_secs(15 * 60),
            sweep_every: Duration::from_secs(30),
            backoff_base: Duration::from_secs(5),
            backoff_max: Duration::from_secs(5 * 60),
            store_fatal_tenants: 3,
            store_window: Duration::from_secs(60),
        }
    }
}

/// Doubling backoff, capped. The first failure waits `base`, the second
/// twice that, and so on until `max`.
pub(super) fn backoff_for(attempts: u32, base: Duration, max: Duration) -> Duration {
    let shift = attempts.saturating_sub(1).min(16);
    base.saturating_mul(1u32 << shift).min(max)
}

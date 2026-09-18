//! What one company cost, counted where the registry already stands.
//!
//! The engine is not asked to report anything: the registry owns the
//! channel its engine drains, so wrapping that channel is all the counting
//! there is.

use super::locked;
use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What one company cost and is costing, countable from where the registry
/// stands (plan B8).
///
/// The registry sees a company's traffic because it owns the channel its
/// engine drains, so `turns` and `in_flight` are counted by wrapping it.
/// `requests` is the engine's own number, reported when a metered run ends
/// on its ceiling. Nothing here is sampled or estimated.
#[derive(Debug, Default)]
pub(super) struct TenantStats {
    turns: AtomicU64,
    pub(super) failures: AtomicU64,
    replies: AtomicU64,
    pub(super) builds: AtomicU64,
    requests: AtomicU64,
}

impl TenantStats {
    /// Messages this company's engine has taken off its queue.
    pub(super) fn turns(&self) -> u64 {
        self.turns.load(Ordering::SeqCst)
    }
    /// Replies it has sent back.
    pub(super) fn replies(&self) -> u64 {
        self.replies.load(Ordering::SeqCst)
    }
    /// Turns taken and not yet answered. A difference rather than a gauge,
    /// so a turn that fails without replying stays counted until the
    /// company is rebuilt - which is the honest reading of "in flight".
    pub(super) fn in_flight(&self) -> u64 {
        self.turns().saturating_sub(self.replies())
    }
    /// Builds that failed, plus runs that ended with an error.
    pub(super) fn failures(&self) -> u64 {
        self.failures.load(Ordering::SeqCst)
    }
    /// Engines built for this company since the process started.
    pub(super) fn builds(&self) -> u64 {
        self.builds.load(Ordering::SeqCst)
    }
    /// Provider requests attributed to this company, as the engine counted
    /// them (`EngineError::RequestCap`'s `spent`).
    pub(super) fn requests(&self) -> u64 {
        self.requests.load(Ordering::SeqCst)
    }

    pub(super) fn record_spend(&self, requests: u32) {
        self.requests
            .fetch_add(u64::from(requests), Ordering::SeqCst);
    }
}

/// The channel handed to one company's engine, counting what crosses it.
///
/// The engine is not asked to report anything: a turn is a message taken
/// off this company's queue, and the count is taken where the registry
/// already stands between the listener and the engine.
pub(super) struct CountedChannel {
    pub(super) inner: Arc<dyn Channel>,
    pub(super) stats: Arc<TenantStats>,
    pub(super) activity: Arc<Activity>,
}

#[async_trait]
impl Channel for CountedChannel {
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        let got = self.inner.recv().await;
        if got.is_ok() {
            self.stats.turns.fetch_add(1, Ordering::SeqCst);
            self.activity.touch();
        }
        got
    }

    async fn send(&self, session: &SessionId, text: &str) -> Result<(), ChannelError> {
        let sent = self.inner.send(session, text).await;
        if sent.is_ok() {
            self.stats.replies.fetch_add(1, Ordering::SeqCst);
            self.activity.touch();
        }
        sent
    }
}

/// When this company last said or heard anything.
pub(super) struct Activity(Mutex<Instant>);

impl Activity {
    pub(super) fn new() -> Self {
        Self(Mutex::new(Instant::now()))
    }
    fn touch(&self) {
        *locked(&self.0) = Instant::now();
    }
    pub(super) fn idle_for(&self) -> Duration {
        locked(&self.0).elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In flight is what has been taken and not yet answered: a company
    /// mid-turn shows one, not zero.
    #[tokio::test]
    async fn a_turn_in_progress_counts_as_in_flight() {
        let stats = Arc::new(TenantStats::default());
        stats.turns.fetch_add(2, Ordering::SeqCst);
        stats.replies.fetch_add(1, Ordering::SeqCst);
        assert_eq!(stats.in_flight(), 1);
        stats.replies.fetch_add(1, Ordering::SeqCst);
        assert_eq!(stats.in_flight(), 0);
    }
}

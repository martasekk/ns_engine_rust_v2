//! One company's side of the listener: the channel its engine drains.

use std::sync::Arc;

use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use tokio::sync::{mpsc, Mutex};

use crate::outbound::deliver;
use crate::shared::Shared;

/// The engine's side of one company: its own inbound queue, and the one
/// outbound map every reply goes back through — sessions are unique across
/// companies by construction (`nsidentity` shapes them `<tenant>/…`), so
/// one map cannot cross a reply from one company into another.
pub struct TenantChannel {
    tenant: String,
    shared: Arc<Shared>,
    /// One `recv` at a time, as the dispatcher guarantees; the lock is what
    /// lets the trait's `&self` hold.
    inbound: Mutex<mpsc::Receiver<Incoming>>,
}

impl TenantChannel {
    /// The only way to one of these, so a company's receiver is reachable
    /// from nowhere but this module: the listener hands over what it took
    /// out of the queue and holds none of it back.
    pub(crate) fn new(
        tenant: String,
        shared: Arc<Shared>,
        inbound: mpsc::Receiver<Incoming>,
    ) -> Self {
        Self {
            tenant,
            shared,
            inbound: Mutex::new(inbound),
        }
    }
}

#[async_trait]
impl Channel for TenantChannel {
    /// This company's messages and no other's. `Closed` once the listener
    /// has been dropped *and* this queue has drained: the shutdown clears
    /// the queues, which drops the senders, which is what ends this — so a
    /// message already queued is still delivered, exactly as when one
    /// sender lived in the accept loop.
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        self.inbound
            .lock()
            .await
            .recv()
            .await
            .ok_or(ChannelError::Closed)
    }

    async fn send(&self, session: &SessionId, text: &str) -> Result<(), ChannelError> {
        deliver(&self.shared, session, text);
        Ok(())
    }
}

impl Drop for TenantChannel {
    /// The receiver goes back where it came from, so an evicted company's
    /// queue is picked up again rather than lost, and anything that arrived
    /// while this channel was being dropped wakes whoever is watching.
    fn drop(&mut self) {
        // A receiver to leave in its place: the real one has to move out,
        // and this one is closed the moment it is dropped with it.
        let (_unused, dead) = mpsc::channel(1);
        let rx = std::mem::replace(self.inbound.get_mut(), dead);
        let mut queues = self.shared.queues();
        // No entry means the listener was dropped first: there is nothing
        // left to park it in, and nothing left to drain it either.
        let Some(queue) = queues.get_mut(&self.tenant) else {
            return;
        };
        let waiting = !rx.is_empty();
        queue.parked = Some(rx);
        queue.woken = false;
        if waiting {
            Shared::wake(&mut queues, &self.shared.wake, &self.tenant);
        }
    }
}

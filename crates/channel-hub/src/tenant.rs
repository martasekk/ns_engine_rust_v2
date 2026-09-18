//! One company's side of the hub: the channel its engine drains.

use std::sync::Arc;

use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use tokio::sync::{mpsc, Mutex};

use crate::hub::Hub;
use crate::outbound::deliver;

/// The engine's side of one company: its own inbound queue, and the one sink
/// map every reply goes back through — sessions are unique across companies
/// by construction (`nsidentity` shapes them `<tenant>/<channel>/<subject>`),
/// so one map cannot cross a reply from one company into another.
pub struct TenantChannel {
    tenant: String,
    hub: Arc<Hub>,
    /// One `recv` at a time, as the dispatcher guarantees; the lock is what
    /// lets the trait's `&self` hold.
    inbound: Mutex<mpsc::Receiver<Incoming>>,
}

impl TenantChannel {
    /// The only way to one of these, so a company's receiver is reachable
    /// from nowhere but this module: [`Hub::tenant_channel`] hands over what
    /// it took out of the queue and holds none of it back.
    pub(crate) fn new(tenant: String, hub: Arc<Hub>, inbound: mpsc::Receiver<Incoming>) -> Self {
        Self {
            tenant,
            hub,
            inbound: Mutex::new(inbound),
        }
    }

    /// Which company this is. The registry labels its counters with it.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }
}

#[async_trait]
impl Channel for TenantChannel {
    /// This company's messages and no other's, whichever way in they
    /// arrived by. `Closed` once the hub has shut down *and* this queue has
    /// drained: the shutdown clears the queues, which drops the senders,
    /// which is what ends this — so a message already queued is still
    /// delivered.
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        self.inbound
            .lock()
            .await
            .recv()
            .await
            .ok_or(ChannelError::Closed)
    }

    async fn send(&self, session: &SessionId, text: &str) -> Result<(), ChannelError> {
        deliver(&self.hub, session, text);
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
        let mut queues = self.hub.queues();
        // No entry means the hub shut down first: there is nothing left to
        // park it in, and nothing left to drain it either.
        let Some(queue) = queues.get_mut(&self.tenant) else {
            return;
        };
        let waiting = !rx.is_empty();
        queue.parked = Some(rx);
        queue.woken = false;
        if waiting {
            self.hub.wake(&mut queues, &self.tenant);
        }
    }
}

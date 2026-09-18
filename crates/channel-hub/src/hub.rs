//! The state every way in shares, and the one thing that ends them all.
//!
//! A field here belongs to the module that owns what it holds: the sink map
//! to [`crate::outbound`], the queues and the wake stream to
//! [`crate::queues`]. This file is where they sit together, where a poisoned
//! lock is recovered from rather than propagated, and where the shutdown is
//! counted — because with two ingresses on one hub, "the listener was
//! dropped" is no longer a sentence about one socket.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use nscore::{Incoming, SessionId};
use tokio::sync::futures::Notified;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::outbound::Sink;
use crate::queues::Queue;
use crate::tenant::TenantChannel;

/// One process's meeting point: every company's queue, every session's
/// windows, and the shutdown all of them observe.
pub struct Hub {
    /// This hub, as the things it hands out hold it. Set by `new`, and the
    /// only thing in here that is not state.
    me: std::sync::Weak<Hub>,
    /// Which sinks hold each session, and the way to write to each. A
    /// session with no holder has no entry at all.
    pub(crate) sinks: StdMutex<HashMap<SessionId, Vec<Sink>>>,
    /// One queue per company, created by that company's first message and
    /// cleared by the shutdown.
    pub(crate) queues: StdMutex<HashMap<String, Queue>>,
    /// Companies with messages and no engine draining them. Unbounded is
    /// safe because it holds names, not messages: an ingress refuses a
    /// company its resolver does not know, so no queue can exist for one,
    /// and `woken` allows each configured company at most one name in
    /// flight. The bound is therefore the startup-validated set.
    ///
    /// In a slot, and never cloned, because dropping it is what ends the
    /// registry's loop: the shutdown takes it, the receiver drains what it
    /// holds and then yields `None`, which is the shard's "no more
    /// companies will speak" and the only way that loop ends by itself.
    wake: StdMutex<Option<mpsc::UnboundedSender<String>>>,
    /// The other end of it, held by whoever runs the registry.
    wake_rx: Mutex<mpsc::UnboundedReceiver<String>>,
    /// Numbers sinks, so a window that goes removes its own entry and
    /// leaves the others holding the same session alone.
    next_sink: AtomicU64,
    /// How many ways in are open. The last one closed is the shutdown: a
    /// shard with a socket and a webhook endpoint must not stop serving
    /// either because the other was dropped.
    ingresses: AtomicUsize,
    /// Shutdown, read before every await that could otherwise block for
    /// ever: an accept, a read, a send into a full queue. Set before the
    /// notify, and every waiter registers on the notify before it reads the
    /// flag, so neither order loses the wakeup.
    pub(crate) closed: AtomicBool,
    pub(crate) shutdown: Notify,
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        let (wake, wake_rx) = mpsc::unbounded_channel();
        // Cyclic because two of the things a hub hands out hold it: a tenant
        // channel, which parks its receiver back on drop, and a sink handle,
        // which removes its own entry. Keeping the reference here rather
        // than asking every caller for `&Arc<Self>` is what lets the
        // registry name the hub behind a `dyn` trait.
        Arc::new_cyclic(|me| Hub {
            me: me.clone(),
            sinks: StdMutex::new(HashMap::new()),
            queues: StdMutex::new(HashMap::new()),
            wake: StdMutex::new(Some(wake)),
            wake_rx: Mutex::new(wake_rx),
            next_sink: AtomicU64::new(0),
            ingresses: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            shutdown: Notify::new(),
        })
    }

    /// Registers one way in. Hold the handle for as long as that ingress
    /// serves: dropping the last one shuts the hub down, which is how a
    /// dropped listener has always been this process's shutdown.
    pub fn ingress(&self) -> Ingress {
        self.ingresses.fetch_add(1, Ordering::SeqCst);
        Ingress { hub: self.me() }
    }

    /// This hub as an owning handle. Never `None`: the only moments the
    /// weak reference cannot be upgraded are inside `new`, before anyone has
    /// the hub, and inside its drop, after the last holder let go.
    pub(crate) fn me(&self) -> Arc<Hub> {
        self.me
            .upgrade()
            .expect("the hub outlives what it hands out")
    }

    /// One company's side of the hub: the channel its engine drains, fed by
    /// every ingress that resolved a caller into that company.
    ///
    /// `None` when the company has never spoken (no queue yet) or when
    /// somebody already holds its receiver — a company has one engine, and
    /// two drains of one queue would split its conversation in half.
    /// Dropping the returned channel parks the receiver again, so the
    /// registry must drop it **before** it unregisters the company:
    /// reversed, a resolve racing the teardown finds the queue still there
    /// with no receiver in it and hands back nothing.
    pub fn tenant_channel(&self, tenant: &str) -> Option<Arc<TenantChannel>> {
        let inbound = crate::queues::take_parked(self, tenant)?;
        Some(Arc::new(TenantChannel::new(
            tenant.to_string(),
            self.me(),
            inbound,
        )))
    }

    /// The next company with messages and no engine draining them: a
    /// company speaking for the first time, or one whose engine was evicted
    /// and has been spoken to again. Each is named once until its receiver
    /// is taken, so a flood is one wake and not a thousand.
    ///
    /// `None` only once the hub is shut down and the wake stream with it.
    pub async fn next_active_tenant(&self) -> Option<String> {
        self.wake_rx.lock().await.recv().await
    }

    /// True once every ingress has gone, or once [`shutdown`](Self::shutdown)
    /// was called. Read *after* registering on [`notified`](Self::notified)
    /// and before any await that could otherwise park for ever.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// The shutdown wakeup. Register on it, enable it, *then* check
    /// [`is_closed`](Self::is_closed): in that order a shutdown on either
    /// side of the check is still seen.
    pub fn notified(&self) -> Notified<'_> {
        self.shutdown.notified()
    }

    /// Ends the hub: no new queue can be created, every tenant channel
    /// reports closed once it has drained what it already had, and every
    /// task parked on [`notified`](Self::notified) wakes.
    ///
    /// Called for you when the last [`Ingress`] is dropped. Calling it
    /// directly is for a shard that wants to stop while its sockets are
    /// still open.
    pub fn shutdown(&self) {
        let mut queues = self.queues();
        // Under the queues lock, because `sender_for` checks the flag under
        // it too: a queue created after this clear would be one nothing will
        // ever drain.
        self.closed.store(true, Ordering::SeqCst);
        queues.clear();
        // The wake stream ends here, and it is the only copy of the sender:
        // whoever is parked on `next_active_tenant` gets `None`, which is
        // how the registry's loop learns the shard is over.
        let sender = self.wake.lock().unwrap_or_else(|p| p.into_inner()).take();
        drop(queues);
        drop(sender);
        self.shutdown.notify_waiters();
    }

    /// Says a company has messages nobody is draining, at most once until
    /// somebody takes its receiver. The caller holds the queues lock, which
    /// is what makes "at most once" true.
    pub(crate) fn wake(&self, queues: &mut HashMap<String, Queue>, tenant: &str) {
        let Some(queue) = queues.get_mut(tenant) else {
            return;
        };
        if queue.parked.is_some() && !queue.woken {
            queue.woken = true;
            // Gone means the hub is shut down, and so is everything that
            // would have drained this.
            if let Some(wake) = self.wake.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                let _ = wake.send(tenant.to_string());
            }
        }
    }

    /// The sink map, whatever a panic elsewhere left behind. A panic while
    /// an ingress held this lock poisons it, and propagating that would turn
    /// one failed task into every later reply panicking — across every
    /// company, since one hub is behind all of them. The map is a table of
    /// senders and every update through it is a single insert or remove, so
    /// what a panic can leave is a value, never half of one.
    pub(crate) fn sinks(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Vec<Sink>>> {
        self.sinks.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The queues, recovered from a poison for the same reason as
    /// [`sinks`](Self::sinks): one panicking task must not take every
    /// company's traffic with it.
    pub(crate) fn queues(&self) -> std::sync::MutexGuard<'_, HashMap<String, Queue>> {
        self.queues.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn next_sink_id(&self) -> u64 {
        self.next_sink.fetch_add(1, Ordering::SeqCst)
    }

    /// Hands one message to its company's queue, waiting while that queue is
    /// full — backpressure on that company's own callers, never a drop.
    /// False when the caller is finished with: the queue has gone, or the
    /// hub is shutting down, which must not wait on a company nobody is
    /// draining.
    pub async fn hand_over(&self, tx: &mpsc::Sender<Incoming>, incoming: Incoming) -> bool {
        let mut shutdown = Box::pin(self.notified());
        shutdown.as_mut().enable();
        if self.is_closed() {
            return false;
        }
        tokio::select! {
            // Biased, so a message that has already been read off the wire
            // still lands when its queue has room: the shutdown ends what is
            // *waiting*, and what a tenant channel already has it still
            // drains.
            biased;
            sent = tx.send(incoming) => sent.is_ok(),
            _ = &mut shutdown => false,
        }
    }
}

/// One way in, open. Dropping the last one is the shutdown.
///
/// It holds the hub rather than borrowing it so that an ingress can be
/// spawned onto a task of its own and still be the thing whose life the
/// shard's shutdown follows.
pub struct Ingress {
    hub: Arc<Hub>,
}

impl Ingress {
    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }
}

impl Drop for Ingress {
    fn drop(&mut self) {
        if self.hub.ingresses.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.hub.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shutdown is the *last* way in closing, not the first. Before the
    /// hub there was one listener and dropping it was the shutdown; a shard
    /// that serves a socket and a webhook endpoint must not lose the webhook
    /// because the socket was dropped.
    #[tokio::test]
    async fn the_last_ingress_out_shuts_the_hub_down() {
        let hub = Hub::new();
        let socket = hub.ingress();
        let webhook = hub.ingress();
        assert!(!hub.is_closed());
        drop(socket);
        assert!(!hub.is_closed(), "one way in left is still a way in");
        drop(webhook);
        assert!(hub.is_closed(), "none left is the shutdown");
        assert!(
            hub.next_active_tenant().await.is_none(),
            "and the wake stream ends with it"
        );
    }

    /// One ingress behaves exactly as the single listener always did.
    #[tokio::test]
    async fn one_ingress_dropped_is_the_shutdown() {
        let hub = Hub::new();
        let only = hub.ingress();
        assert!(crate::queues::sender_for(&hub, "acme").is_some());
        drop(only);
        assert!(hub.is_closed());
        assert!(
            crate::queues::sender_for(&hub, "acme").is_none(),
            "no queue may be created after the clear"
        );
    }
}

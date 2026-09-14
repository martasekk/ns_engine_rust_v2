//! One inbound queue per company, and the stream that names the companies
//! nobody is draining.
//!
//! Nothing outside this module reaches the queue map: a company's messages
//! are put in through [`sender_for`], taken out through [`take_parked`],
//! and announced through [`Shared::wake`].

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use nscore::Incoming;
use tokio::sync::mpsc;

use crate::shared::Shared;

/// Messages waiting for one company's engine, across every connection of
/// that company. Full means those connection tasks stop reading their
/// sockets until it drains — backpressure, never a drop, and never a
/// company's flood held against another's queue.
pub const INBOUND_DEPTH: usize = 64;
/// One company's queue. `parked` holds the receiving end while no engine
/// has it: a company's first message creates the queue and parks it there,
/// [`TcpChannel::tenant_channel`](crate::TcpChannel::tenant_channel) takes
/// it out, and the tenant channel's
/// drop puts it back, so a company that is evicted and then spoken to
/// again finds its messages where it left them.
pub(crate) struct Queue {
    pub(crate) tx: mpsc::Sender<Incoming>,
    pub(crate) parked: Option<mpsc::Receiver<Incoming>>,
    /// A wake for this company is outstanding and nobody has taken the
    /// receiver since. It is what keeps the wake stream to one name per
    /// company however many messages arrive.
    pub(crate) woken: bool,
}

impl Shared {
    /// Says a company has messages nobody is draining, at most once until
    /// somebody takes its receiver. The caller holds the queues lock.
    pub(crate) fn wake(
        queues: &mut HashMap<String, Queue>,
        wake: &mpsc::UnboundedSender<String>,
        tenant: &str,
    ) {
        let Some(queue) = queues.get_mut(tenant) else {
            return;
        };
        if queue.parked.is_some() && !queue.woken {
            queue.woken = true;
            // The receiver lives in the listener handle; if that has gone,
            // so has everything that would have drained this.
            let _ = wake.send(tenant.to_string());
        }
    }
}

/// Takes a company's parked receiver, if its queue exists and nobody holds
/// it already. Taking it consumes the outstanding wake: whoever holds the
/// receiver sees the messages itself and needs no telling.
pub(crate) fn take_parked(shared: &Shared, tenant: &str) -> Option<mpsc::Receiver<Incoming>> {
    let mut queues = shared.queues();
    let queue = queues.get_mut(tenant)?;
    let rx = queue.parked.take()?;
    queue.woken = false;
    Some(rx)
}

/// The sender for this company's queue, creating the queue if this is the
/// first message that company has ever sent. Looked up per message and
/// never held: a connection that is blocked on a full queue is then
/// blocked on that company's queue as it stands now, so a flood delays
/// that company's own connections and nothing else, and a queue whose
/// engine has just been evicted and rebuilt is picked up without the
/// connection noticing. It is a map lookup and a clone under an
/// uncontended lock, against a turn that takes seconds.
pub(crate) fn sender_for(shared: &Shared, tenant: &str) -> Option<mpsc::Sender<Incoming>> {
    let mut queues = shared.queues();
    // Under the lock, as the shutdown clears the map under it too: a queue
    // created after that clear would be one nothing will ever drain.
    if shared.closed.load(Ordering::SeqCst) {
        return None;
    }
    let tx = queues
        .entry(tenant.to_string())
        .or_insert_with(|| {
            let (tx, rx) = mpsc::channel(INBOUND_DEPTH);
            Queue {
                tx,
                parked: Some(rx),
                woken: false,
            }
        })
        .tx
        .clone();
    // Nobody holds this company's receiver: a cold company, or one whose
    // engine was evicted. Say so once.
    Shared::wake(&mut queues, &shared.wake, tenant);
    Some(tx)
}

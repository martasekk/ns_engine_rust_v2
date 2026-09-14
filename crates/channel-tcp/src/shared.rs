//! The state the accept loop, every connection and every channel share.
//!
//! A field here belongs to the module that owns what it holds: the outbound
//! map to [`crate::outbound`], the queues and the wake stream to
//! [`crate::queues`]. This file is where they sit together, and the two
//! accessors are where a poisoned lock is recovered from rather than
//! propagated.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nscore::SessionId;
use nsidentity::{Hello, IdentityResolver};
use tokio::sync::{mpsc, Notify};

use crate::outbound::Outbound;
use crate::queues::Queue;

/// What the accept loop and the connection tasks share with the channel.
pub(crate) struct Shared {
    /// Turns a hello into an identity, or refuses it. The only thing here
    /// that knows what a credential is.
    pub(crate) resolver: Arc<dyn IdentityResolver<Hello>>,
    pub(crate) hello_timeout: Duration,
    pub(crate) max_connections: usize,
    /// Live connections, machine-wide.
    pub(crate) live: AtomicUsize,
    /// Numbers connections, so one that hangs up removes its own entry and
    /// leaves the others holding the same session alone.
    pub(crate) next_conn: AtomicU64,
    /// Which connections hold each session, and the way to write to each.
    /// A session with no holder has no entry at all.
    pub(crate) outbound: StdMutex<HashMap<SessionId, Vec<Outbound>>>,
    /// One queue per company, created by that company's first message and
    /// cleared when the listener is dropped.
    pub(crate) queues: StdMutex<HashMap<String, Queue>>,
    /// Companies with messages and no engine draining them. Unbounded is
    /// safe because it holds names, not messages: identity refuses a
    /// company that was not configured at startup, so no queue can exist
    /// for one, and `woken` allows each configured company at most one
    /// name in flight. The bound is therefore the startup-validated set.
    pub(crate) wake: mpsc::UnboundedSender<String>,
    /// Shutdown, read before every await that could otherwise block for
    /// ever: the accept, each connection's read, and a send into a full
    /// queue. Set before the notify, and every waiter registers on the
    /// notify before it reads the flag, so neither order loses the wakeup.
    pub(crate) closed: AtomicBool,
    pub(crate) shutdown: Notify,
}

impl Shared {
    /// The outbound map, whatever a panic elsewhere left behind. A panic
    /// while a connection held this lock poisons it, and propagating that
    /// would turn one failed task into every later reply panicking — across
    /// every company, once one listener is behind all of them. The map is a
    /// table of senders and every update through it is a single insert or
    /// remove, so what a panic can leave is a value, never half of one.
    pub(crate) fn outbound(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Vec<Outbound>>> {
        self.outbound.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The queues, recovered from a poison for the same reason as
    /// [`outbound`](Self::outbound): one panicking task must not take every
    /// company's traffic with it.
    pub(crate) fn queues(&self) -> std::sync::MutexGuard<'_, HashMap<String, Queue>> {
        self.queues.lock().unwrap_or_else(|p| p.into_inner())
    }
}

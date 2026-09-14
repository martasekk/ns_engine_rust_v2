//! Which sinks hold each session, and the one way a reply reaches them.
//!
//! Nothing outside this module reaches the sink map: [`deliver`] is the only
//! way to a sink, so "a reply goes to every window on its session, and a
//! peer that has stopped reading loses its own" is one function rather than
//! a rule to remember.
//!
//! A *sink* is deliberately less than a connection. It is a bounded queue of
//! reply text and nothing else, so a browser's WebSocket, a desktop app's
//! TCP socket and the task that posts a reply back to WhatsApp are the same
//! thing here. The ingress that attached it decides what the text becomes on
//! its own wire.

use std::sync::Arc;

use nscore::SessionId;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::hub::Hub;

/// Replies waiting to be written to one window. Full means that peer has
/// stopped reading; further replies are logged and dropped, as for a peer
/// that has gone.
pub const OUTBOUND_DEPTH: usize = 64;

pub(crate) struct Sink {
    pub(crate) id: u64,
    pub(crate) tx: mpsc::Sender<String>,
}

/// One window's claim on a session, released when it is dropped.
///
/// Holding this is what makes a session reachable. Dropping it removes that
/// one sink and leaves every other window on the same session alone, which
/// is the whole of "a second tab is not an impostor" (multi-tenant plan
/// Phase 5, hazard H7) — and it is a guard rather than a method pair so that
/// a connection task ending on any path, including a panic, still lets go.
pub struct SinkHandle {
    hub: Arc<Hub>,
    session: SessionId,
    id: u64,
}

impl SinkHandle {
    /// How many windows hold this session, this one included. An ingress
    /// logs it; nothing depends on it.
    pub fn holders(&self) -> usize {
        self.hub.sinks().get(&self.session).map_or(0, Vec::len)
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

impl Drop for SinkHandle {
    fn drop(&mut self) {
        let mut map = self.hub.sinks();
        if let Some(holders) = map.get_mut(&self.session) {
            holders.retain(|s| s.id != self.id);
            // A session with no holder has no entry at all: the map is the
            // set of reachable sessions, not a record of every session that
            // ever spoke.
            if holders.is_empty() {
                map.remove(&self.session);
            }
        }
    }
}

impl Hub {
    /// Attaches one window to a session and hands back the receiver its
    /// replies will arrive on.
    ///
    /// Joining, not taking over: the sinks already there keep their
    /// receivers, and so their windows. The caller drains the receiver and
    /// holds the [`SinkHandle`] for exactly as long as it can write.
    pub fn attach(&self, session: &SessionId) -> (SinkHandle, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(OUTBOUND_DEPTH);
        let id = self.next_sink_id();
        self.sinks()
            .entry(session.clone())
            .or_default()
            .push(Sink { id, tx });
        (
            SinkHandle {
                hub: self.me(),
                session: session.clone(),
                id,
            },
            rx,
        )
    }
}

/// One reply to every window holding its session, whichever company asked
/// for it and whichever way in each window came.
pub(crate) fn deliver(hub: &Hub, session: &SessionId, text: &str) {
    // Cloned out from under the lock: the lock is a std one, and what
    // follows is per window.
    let holders: Vec<mpsc::Sender<String>> = hub
        .sinks()
        .get(session)
        .map(|v| v.iter().map(|s| s.tx.clone()).collect())
        .unwrap_or_default();
    if holders.is_empty() {
        eprintln!(
            "hub: no window holds session {}; the reply is in the log only",
            session.0
        );
        return;
    }
    // One queue per window, and `try_send` on each: a peer that has stopped
    // reading loses this reply and delays neither `send` nor the other
    // windows on its session.
    for tx in holders {
        if let Err(e) = tx.try_send(text.to_string()) {
            let why = match e {
                TrySendError::Full(_) => "has stopped reading",
                TrySendError::Closed(_) => "has gone",
            };
            eprintln!(
                "hub: a window holding session {} {why}; the reply is in the log only \
                 for that window",
                session.0
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (B3) One panic while an ingress held the sink lock must not take
    /// every later reply with it. Before the recovery this `deliver`
    /// panicked on the poison, and with one hub behind every company it
    /// would do so for all of them rather than for one task.
    #[tokio::test]
    async fn a_poisoned_sink_lock_still_delivers() {
        let hub = Hub::new();
        let _open = hub.ingress();
        let session = SessionId("s".into());
        let (_held, mut rx) = hub.attach(&session);

        let poisoner = hub.clone();
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.sinks.lock().expect("clean until this panic");
            panic!("a task panicked holding the sink lock");
        });
        assert!(panicked.join().is_err(), "the thread panicked holding it");
        assert!(hub.sinks.is_poisoned(), "the lock is poisoned");

        deliver(&hub, &session, "after the panic");
        assert_eq!(rx.try_recv().ok().as_deref(), Some("after the panic"));
    }

    /// Two windows on one session both hear the reply, and the one that
    /// leaves takes only its own entry with it.
    #[tokio::test]
    async fn a_window_that_leaves_takes_only_its_own_sink() {
        let hub = Hub::new();
        let _open = hub.ingress();
        let session = SessionId("acme/web/u1".into());
        let (first, mut a) = hub.attach(&session);
        let (second, mut b) = hub.attach(&session);
        assert_eq!(first.holders(), 2, "both windows hold it");

        deliver(&hub, &session, "to both");
        assert_eq!(a.try_recv().ok().as_deref(), Some("to both"));
        assert_eq!(b.try_recv().ok().as_deref(), Some("to both"));

        drop(first);
        assert_eq!(second.holders(), 1, "the other window still holds it");
        deliver(&hub, &session, "to the one left");
        assert_eq!(b.try_recv().ok().as_deref(), Some("to the one left"));

        drop(second);
        assert!(
            hub.sinks().get(&session).is_none(),
            "no holder, no entry at all"
        );
    }
}

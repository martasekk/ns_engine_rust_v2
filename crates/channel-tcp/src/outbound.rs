//! Which connections hold each session, and the one way a reply reaches
//! them.
//!
//! Nothing outside this module reaches the outbound map: [`deliver`] is the
//! only way to a connection's writer, so "a reply goes to every window on
//! its session, and a peer that has stopped reading loses its own" is one
//! function rather than a rule to remember.

use nscore::SessionId;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::shared::Shared;

/// Replies waiting to be written to one connection. Full means that peer
/// has stopped reading; further replies are logged and dropped, as for a
/// peer that has gone.
pub(crate) const OUTBOUND_DEPTH: usize = 64;
pub(crate) struct Outbound {
    pub(crate) conn: u64,
    pub(crate) tx: mpsc::Sender<String>,
}

/// One reply to every connection holding its session, whichever company
/// asked for it.
pub(crate) fn deliver(shared: &Shared, session: &SessionId, text: &str) {
    // Cloned out from under the lock: the lock is a std one, and what
    // follows is per connection.
    let holders: Vec<mpsc::Sender<String>> = shared
        .outbound()
        .get(session)
        .map(|v| v.iter().map(|o| o.tx.clone()).collect())
        .unwrap_or_default();
    if holders.is_empty() {
        eprintln!(
            "tcp: no connection holds session {}; the reply is in the log only",
            session.0
        );
        return;
    }
    // One queue per connection, and `try_send` on each: a peer that has
    // stopped reading loses this reply and delays neither `send` nor
    // the other windows on its session.
    for tx in holders {
        if let Err(e) = tx.try_send(text.to_string()) {
            let why = match e {
                TrySendError::Full(_) => "has stopped reading",
                TrySendError::Closed(_) => "has gone",
            };
            eprintln!(
                "tcp: a connection holding session {} {why}; the reply is in the log only \
                 for that window",
                session.0
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TcpChannel;
    use nscore::SessionId;

    /// (B3) One panic while a connection held the outbound lock must not
    /// take every later reply with it. Before the recovery this `send`
    /// panicked on the poison, and with one listener behind many companies
    /// it would do so for all of them rather than for one process.
    #[tokio::test]
    async fn a_poisoned_outbound_lock_still_delivers() {
        let ch = TcpChannel::bind_shared("127.0.0.1:0", "t0k".into(), 2, false)
            .await
            .expect("bind loopback");
        let session = SessionId("s".into());
        let (tx, mut rx) = mpsc::channel(4);
        ch.shared
            .outbound
            .lock()
            .expect("the lock is clean here")
            .insert(session.clone(), vec![Outbound { conn: 0, tx }]);

        let shared = ch.shared.clone();
        let panicked = std::thread::spawn(move || {
            let _held = shared.outbound.lock().expect("clean until this panic");
            panic!("a connection task panicked holding the outbound lock");
        });
        assert!(panicked.join().is_err(), "the thread panicked holding it");
        assert!(ch.shared.outbound.is_poisoned(), "the lock is poisoned");

        deliver(&ch.shared, &session, "after the panic");
        assert_eq!(rx.try_recv().ok().as_deref(), Some("after the panic"));
    }
}

//! The socket: binding it, accepting on it, and one connection's life.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nscore::Incoming;
use nsidentity::{Hello, IdentityResolver, SharedTokenResolver};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};

use crate::outbound::{Outbound, OUTBOUND_DEPTH};
use crate::queues::{sender_for, take_parked};
use crate::shared::Shared;
use crate::tenant::TenantChannel;
use crate::wire::{write_replies, Text, HELLO_MAX, HELLO_TIMEOUT};

/// The company a shared-token connection speaks for. One process, one
/// token, one tenant: the shared token cannot tell two companies apart.
const SHARED_TENANT: &str = "local";

/// Why `bind` refused to start. Each is a mistake that would otherwise fail
/// silently and permanently: a server with no token answers anyone who finds
/// the port, one bound to `0.0.0.0` by accident is the whole conversation on
/// the network, and one that proves nothing about its callers must not be
/// reachable from off the machine at all.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("refusing to start with an empty token: there is no unauthenticated mode")]
    EmptyToken,
    #[error("refusing to serve on {0}: not a loopback address — set allow_remote to mean it")]
    NotLoopback(SocketAddr),
    #[error("refusing to serve on {addr} with {auth}: it proves nothing, so it is loopback only")]
    SharedAuthOffLoopback { addr: SocketAddr, auth: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// The listener itself: one socket, one accept loop, and one queue per
/// company behind it. Dropping it is the shutdown — the accept loop, every
/// connection and every blocked send observe it, and every
/// [`TenantChannel`] reports closed once its own queue has drained.
pub struct TcpChannel {
    local_addr: SocketAddr,
    /// Which company has messages nobody is draining. One name at a time
    /// per company ([`Queue::woken`](crate::queues::Queue)), consumed by
    /// [`next_active_tenant`](Self::next_active_tenant).
    wake_rx: Mutex<mpsc::UnboundedReceiver<String>>,
    /// Crate-visible for the delivery tests in [`crate::outbound`], which
    /// poison the outbound lock on purpose; nothing outside the crate can
    /// reach it.
    pub(crate) shared: Arc<Shared>,
}

impl TcpChannel {
    /// Binds `listen` before returning, so a bad address fails at startup
    /// and not in the accept loop, then spawns the accept loop. `auth` says
    /// who each hello is; a non-loopback address is refused unless
    /// `allow_remote`, and a resolver that proves nothing is refused off
    /// loopback whatever `allow_remote` says.
    pub async fn bind(
        listen: &str,
        auth: Arc<dyn IdentityResolver<Hello>>,
        max_connections: usize,
        allow_remote: bool,
    ) -> Result<Arc<TcpChannel>, BindError> {
        Self::bind_with(listen, auth, max_connections, allow_remote, HELLO_TIMEOUT).await
    }

    /// [`bind`](Self::bind) with the hello deadline named rather than
    /// [`HELLO_TIMEOUT`].
    pub async fn bind_with(
        listen: &str,
        auth: Arc<dyn IdentityResolver<Hello>>,
        max_connections: usize,
        allow_remote: bool,
        hello_timeout: Duration,
    ) -> Result<Arc<TcpChannel>, BindError> {
        // Before the port is taken: a name resolving to anything off loopback
        // is a mistake worth reporting with the port still free. A `:0` bind
        // is named here by the port that was asked for, which is 0, and the
        // address is what makes the message useful either way.
        for addr in resolve(listen).await? {
            check_address(addr, auth.as_ref(), allow_remote)?;
        }
        let listener = TcpListener::bind(listen).await?;
        let local_addr = listener.local_addr()?;
        // Belt and braces: what was resolved and what was bound are two
        // lookups, and the one that matters is the one holding the socket.
        check_address(local_addr, auth.as_ref(), allow_remote)?;
        let (wake, wake_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            resolver: auth,
            hello_timeout,
            max_connections,
            live: AtomicUsize::new(0),
            next_conn: AtomicU64::new(0),
            outbound: StdMutex::new(HashMap::new()),
            queues: StdMutex::new(HashMap::new()),
            wake,
            closed: AtomicBool::new(false),
            shutdown: Notify::new(),
        });
        tokio::spawn(accept_loop(listener, shared.clone()));
        Ok(Arc::new(Self {
            local_addr,
            wake_rx: Mutex::new(wake_rx),
            shared,
        }))
    }

    /// [`bind`](Self::bind) under the one shared token: every client that
    /// knows it names its own session, which proves nothing, so this is
    /// loopback-only development and the existing tests.
    pub async fn bind_shared(
        listen: &str,
        token: String,
        max_connections: usize,
        allow_remote: bool,
    ) -> Result<Arc<TcpChannel>, BindError> {
        if token.is_empty() {
            return Err(BindError::EmptyToken);
        }
        let auth = Arc::new(SharedTokenResolver::new(token, SHARED_TENANT));
        Self::bind(listen, auth, max_connections, allow_remote).await
    }

    /// Where the listener actually is — the port, when `listen` said `:0`.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// One company's side of this listener: the channel its engine drains,
    /// fed by that company's connections and by nobody else's.
    ///
    /// `None` when the company has never spoken (no queue yet) or when
    /// somebody already holds its receiver — a company has one engine, and
    /// two drains of one queue would split its conversation in half.
    /// Dropping the returned channel parks the receiver again, so the
    /// registry must drop it **before** it unregisters the company:
    /// reversed, a resolve racing the teardown finds the queue still there
    /// with no receiver in it and hands back nothing.
    pub fn tenant_channel(&self, tenant: &str) -> Option<Arc<TenantChannel>> {
        let inbound = take_parked(&self.shared, tenant)?;
        Some(Arc::new(TenantChannel::new(
            tenant.to_string(),
            self.shared.clone(),
            inbound,
        )))
    }

    /// The next company with messages and no engine draining them: a
    /// company speaking for the first time, or one whose engine was
    /// evicted and has been spoken to again. Each is named once until its
    /// receiver is taken, so a flood is one wake and not a thousand.
    ///
    /// Yields nothing rather than `None` while this handle is alive, since
    /// holding it is what keeps the listener up; `None` only if the wake
    /// stream itself has gone.
    pub async fn next_active_tenant(&self) -> Option<String> {
        self.wake_rx.lock().await.recv().await
    }
}

impl Drop for TcpChannel {
    /// Shutdown. The flag goes up under the queues lock, so no connection
    /// can create a queue after the clear; the queues are cleared, so every
    /// tenant channel reports closed once it has drained; and the notify
    /// wakes the accept loop, every connection's read and every send
    /// blocked on a full queue, so one stalled company cannot hold the
    /// shutdown up.
    fn drop(&mut self) {
        let mut queues = self.shared.queues();
        self.shared.closed.store(true, Ordering::SeqCst);
        queues.clear();
        drop(queues);
        self.shared.shutdown.notify_waiters();
    }
}

/// Every address `listen` stands for. A name with two records is two chances
/// to be on the network by accident, so all of them are checked.
async fn resolve(listen: &str) -> Result<Vec<SocketAddr>, BindError> {
    Ok(tokio::net::lookup_host(listen).await?.collect())
}

fn check_address(
    addr: SocketAddr,
    auth: &dyn IdentityResolver<Hello>,
    allow_remote: bool,
) -> Result<(), BindError> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    if !allow_remote {
        return Err(BindError::NotLoopback(addr));
    }
    if auth.loopback_only() {
        return Err(BindError::SharedAuthOffLoopback {
            addr,
            auth: auth.describe().to_string(),
        });
    }
    Ok(())
}

/// Accept until the listener fails or the channel is dropped, one task per
/// connection. The cap is counted here, before the task exists, so it is
/// the count of connections and not of sessions.
async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        // Registered on the notify before the flag is read, so a shutdown
        // on either side of this line is seen: already up, and the flag
        // says so; set afterwards, and the notify reaches a waiter that is
        // already there.
        let mut shutdown = Box::pin(shared.shutdown.notified());
        shutdown.as_mut().enable();
        if shared.closed.load(Ordering::SeqCst) {
            return;
        }
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            // The listener was dropped: nothing would read what a new
            // connection sends, and holding the port would be a lie.
            _ = &mut shutdown => return,
        };
        let (stream, peer) = match accepted {
            Ok(a) => a,
            Err(e) => {
                eprintln!("tcp: accept failed, no longer listening: {e}");
                return;
            }
        };
        let _ = stream.set_nodelay(true);

        let live = shared.live.fetch_add(1, Ordering::SeqCst) + 1;
        if live > shared.max_connections {
            shared.live.fetch_sub(1, Ordering::SeqCst);
            eprintln!(
                "tcp: refusing {peer}: {} connections already",
                shared.max_connections
            );
            drop(stream);
            continue;
        }
        let conn = shared.next_conn.fetch_add(1, Ordering::SeqCst);
        let shared = shared.clone();
        tokio::spawn(async move {
            connection(&shared, stream, peer, conn).await;
            shared.live.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// One connection: the hello, then lines in until EOF. Lives exactly as
/// long as its writer does — which ends when its own entry is released or
/// the socket stops taking replies — and another connection joining the
/// same session touches neither.
async fn connection(shared: &Shared, stream: TcpStream, peer: SocketAddr, conn: u64) {
    let (r, w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    // The hello, under a deadline and a length bound: a peer that says
    // nothing must not hold a connection slot, and one that says too much
    // must not be buffered while it does it. Anything short of a valid hello
    // closes the connection with nothing sent — a caller that failed it
    // learns nothing about why, and the log says which refusal it was.
    let mut bounded = (&mut reader).take(HELLO_MAX);
    let read = match tokio::time::timeout(shared.hello_timeout, bounded.read_line(&mut line)).await
    {
        Ok(read) => read,
        Err(_) => {
            eprintln!(
                "tcp: {peer}: refused (no hello within {:?})",
                shared.hello_timeout
            );
            return;
        }
    };
    let overlong = bounded.limit() == 0 && !line.ends_with('\n');
    drop(bounded);
    match read {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    if overlong {
        eprintln!("tcp: {peer}: refused (a hello longer than {HELLO_MAX} bytes)");
        return;
    }
    let hello = match serde_json::from_str::<Hello>(line.trim()) {
        Ok(h) => h,
        Err(_) => {
            eprintln!("tcp: {peer}: refused (malformed hello)");
            return;
        }
    };
    // The session is the resolver's to say, never the client's to claim: only
    // the shared-token resolver honours what the hello asked for.
    // The company is the resolver's too, and it is the queue this
    // connection's messages will join. No queue can exist for a company
    // the resolver does not know.
    let (tenant, session) = match shared.resolver.resolve(hello).await {
        Ok(identity) => (identity.tenant, identity.session),
        Err(denied) => {
            eprintln!("tcp: {peer}: refused ({denied})");
            return;
        }
    };

    let (tx, rx) = mpsc::channel(OUTBOUND_DEPTH);
    // Joining, not taking over: the entries already there keep their
    // senders, and so their connections.
    let holders = {
        let mut map = shared.outbound();
        let holders = map.entry(session.clone()).or_default();
        holders.push(Outbound { conn, tx });
        holders.len()
    };
    if holders > 1 {
        eprintln!(
            "tcp: {peer} joins session {} ({holders} windows)",
            session.0
        );
    }
    // Writing is its own task, so a reply never waits behind a read.
    let mut writer = tokio::spawn(write_replies(w, session.clone(), rx));

    loop {
        line.clear();
        // Every await in this loop is preceded by the registration and then
        // the flag, so a shutdown ends the connection whatever it is doing.
        let mut shutdown = Box::pin(shared.shutdown.notified());
        shutdown.as_mut().enable();
        if shared.closed.load(Ordering::SeqCst) {
            break;
        }
        let read = tokio::select! {
            read = reader.read_line(&mut line) => read,
            // The socket refused a reply, or this connection's entry went:
            // it is over, and a half-read line goes with it.
            _ = &mut writer => break,
            _ = &mut shutdown => break,
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let text = match serde_json::from_str::<Text>(trimmed) {
            Ok(t) => t.text,
            Err(e) => {
                eprintln!("tcp: {peer}: ignoring a malformed line ({e})");
                continue;
            }
        };
        let incoming = Incoming {
            session: session.clone(),
            text,
        };
        // Looked up now rather than at the hello, so this waits on the
        // queue this company has at this moment and holds up nobody else.
        let Some(tx) = sender_for(shared, &tenant) else {
            // The listener has gone; nothing would read this.
            break;
        };
        if !handed_over(shared, &tx, incoming).await {
            break;
        }
    }

    // Release this connection's own entry, and the session itself only once
    // no window is left holding it.
    let mut map = shared.outbound();
    if let Some(holders) = map.get_mut(&session) {
        holders.retain(|o| o.conn != conn);
        if holders.is_empty() {
            map.remove(&session);
        }
    }
}

/// Hands one message to its company's queue, waiting while that queue is
/// full — backpressure on this company's sockets, never a drop. False when
/// the connection is over: the queue has gone, or the listener is shutting
/// down, which must not wait on a company nobody is draining.
async fn handed_over(shared: &Shared, tx: &mpsc::Sender<Incoming>, incoming: Incoming) -> bool {
    let mut shutdown = Box::pin(shared.shutdown.notified());
    shutdown.as_mut().enable();
    if shared.closed.load(Ordering::SeqCst) {
        return false;
    }
    tokio::select! {
        // Biased, so a message that has already been read off the socket
        // still lands when its queue has room: the shutdown ends what is
        // *waiting*, and what a tenant channel already has it still
        // drains, which is the property the single inbound queue had.
        biased;
        sent = tx.send(incoming) => sent.is_ok(),
        _ = &mut shutdown => false,
    }
}

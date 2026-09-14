//! A channel with more than one session: TCP, one JSON object per line.
//!
//! The first channel in the tree whose `recv` yields `Incoming`s for more
//! than one session (multi-conversation plan Phase 3; findings §2.4).
//! `CliChannel` pins `"cli"` and `WithDesktop` joins the compose box to that
//! same session; here every connection names its session in its first line,
//! and the dispatcher (`nsengine::dispatch`) runs those sessions on their own
//! tasks. What `ns-app serve` listens on.
//!
//! # Wire
//!
//! One JSON object per line, both ways, UTF-8, `\n`-terminated.
//!
//! - The client's first line: `{"token":"…","session":"…"}`. What that line
//!   proves, and what session it stands for, is the resolver's to say
//!   (`nsidentity::IdentityResolver`): a refusal of any kind closes the
//!   connection with nothing sent back — a caller that failed the hello
//!   learns nothing about why, while the log names which refusal it was.
//!   Under the shared token the client still names its own session, and
//!   under a verified resolver the session is derived from the credential
//!   and the field is ignored, so no client can name itself into somebody
//!   else's conversation. A hello that does not arrive within the deadline,
//!   or that runs past [`HELLO_MAX`], is closed too. A valid hello *joins*
//!   the session:
//!   replies for that session id go to every connection holding it, and a
//!   later connection claiming the same id joins the others rather than
//!   displacing them. One conversation across a user's windows is the
//!   normal case — a second tab is not an impostor — and displacing would
//!   leave two tabs knocking each other offline in a loop (multi-tenant
//!   plan Phase 5, hazard H7).
//! - Then, from the client: `{"text":"…"}` per message. A line that is not
//!   that object is ignored, with one line on stderr naming the peer. EOF
//!   ends the connection's task and releases its slot.
//! - To the client: `{"session":"…","text":"…"}` per reply.
//!
//! # Shape
//!
//! Every connection is its own task (precedent: `serve_listener` in
//! `crates/pointer/src/agent.rs`; the lessons in `docs/windows-handoff.md`
//! §1). Each pushes into the queue of the company its hello named, looked
//! up per message, and one [`TenantChannel`] per company drains its own —
//! so a company whose engine is slow holds up its own sockets and nobody
//! else's, and a full queue is backpressure rather than a drop. A company
//! nobody is draining is announced once on
//! [`TcpChannel::next_active_tenant`], which is how a cold company gets an
//! engine and an evicted one gets it back. Dropping the listener is the
//! shutdown: the accept loop, every connection and every send waiting on a
//! full queue observe it, and each tenant channel reports `Closed` once it
//! has drained what it already had. `send` routes by session id to every
//! connection holding that session, each with its own bounded queue; a
//! reply for a session with no live connection, or for one that has stopped
//! reading, is logged and dropped — the engine's log already has it, and
//! that is what the log is for. One peer that stops reading therefore loses
//! its own replies and nobody else's. The connection cap is machine-wide,
//! not per socket.
//!
//! # Deliberately not here
//!
//! No TLS, and loopback by default: a non-loopback bind is refused unless
//! `allow_remote` says it was meant, and a resolver that proves nothing is
//! refused off loopback whatever `allow_remote` says. Who a connection is
//! belongs to `ns-identity`; this crate knows only that something turned a
//! hello into a session id.

use std::collections::HashMap;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use nsidentity::{Hello, IdentityResolver, SharedTokenResolver};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, Mutex, Notify};

/// Messages waiting for one company's engine, across every connection of
/// that company. Full means those connection tasks stop reading their
/// sockets until it drains — backpressure, never a drop, and never a
/// company's flood held against another's queue.
pub const INBOUND_DEPTH: usize = 64;
/// Replies waiting to be written to one connection. Full means that peer
/// has stopped reading; further replies are logged and dropped, as for a
/// peer that has gone.
const OUTBOUND_DEPTH: usize = 64;
/// How long a connection has to send its hello before it is closed. Without
/// it a silent peer would hold one of `max_connections` for ever.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// The most a hello may be. Applied with `take`, so a longer one is refused
/// without ever being buffered.
pub const HELLO_MAX: u64 = 8 * 1024;
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
    /// per company ([`Queue::woken`]), consumed by
    /// [`next_active_tenant`](Self::next_active_tenant) or by this handle's
    /// own `recv`.
    wake_rx: Mutex<mpsc::UnboundedReceiver<String>>,
    /// The receivers this handle's own `Channel::recv` has adopted, for the
    /// process that runs one engine over the whole socket rather than one
    /// per company. One `recv` at a time, as the dispatcher guarantees.
    legacy: Mutex<Vec<(String, mpsc::Receiver<Incoming>)>>,
    shared: Arc<Shared>,
}

/// One company's queue. `parked` holds the receiving end while no engine
/// has it: a company's first message creates the queue and parks it there,
/// [`TcpChannel::tenant_channel`] takes it out, and the tenant channel's
/// drop puts it back, so a company that is evicted and then spoken to
/// again finds its messages where it left them.
struct Queue {
    tx: mpsc::Sender<Incoming>,
    parked: Option<mpsc::Receiver<Incoming>>,
    /// A wake for this company is outstanding and nobody has taken the
    /// receiver since. It is what keeps the wake stream to one name per
    /// company however many messages arrive.
    woken: bool,
}

/// What the accept loop and the connection tasks share with the channel.
struct Shared {
    /// Turns a hello into an identity, or refuses it. The only thing here
    /// that knows what a credential is.
    resolver: Arc<dyn IdentityResolver<Hello>>,
    hello_timeout: Duration,
    max_connections: usize,
    /// Live connections, machine-wide.
    live: AtomicUsize,
    /// Numbers connections, so one that hangs up removes its own entry and
    /// leaves the others holding the same session alone.
    next_conn: AtomicU64,
    /// Which connections hold each session, and the way to write to each.
    /// A session with no holder has no entry at all.
    outbound: StdMutex<HashMap<SessionId, Vec<Outbound>>>,
    /// One queue per company, created by that company's first message and
    /// cleared when the listener is dropped.
    queues: StdMutex<HashMap<String, Queue>>,
    /// Companies with messages and no engine draining them. Unbounded is
    /// safe because it holds names, not messages: identity refuses a
    /// company that was not configured at startup, so no queue can exist
    /// for one, and `woken` allows each configured company at most one
    /// name in flight. The bound is therefore the startup-validated set.
    wake: mpsc::UnboundedSender<String>,
    /// Shutdown, read before every await that could otherwise block for
    /// ever: the accept, each connection's read, and a send into a full
    /// queue. Set before the notify, and every waiter registers on the
    /// notify before it reads the flag, so neither order loses the wakeup.
    closed: AtomicBool,
    shutdown: Notify,
}

impl Shared {
    /// The outbound map, whatever a panic elsewhere left behind. A panic
    /// while a connection held this lock poisons it, and propagating that
    /// would turn one failed task into every later reply panicking — across
    /// every company, once one listener is behind all of them. The map is a
    /// table of senders and every update through it is a single insert or
    /// remove, so what a panic can leave is a value, never half of one.
    fn outbound(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Vec<Outbound>>> {
        self.outbound.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The queues, recovered from a poison for the same reason as
    /// [`outbound`](Self::outbound): one panicking task must not take every
    /// company's traffic with it.
    fn queues(&self) -> std::sync::MutexGuard<'_, HashMap<String, Queue>> {
        self.queues.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Says a company has messages nobody is draining, at most once until
    /// somebody takes its receiver. The caller holds the queues lock.
    fn wake(
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
fn take_parked(shared: &Shared, tenant: &str) -> Option<mpsc::Receiver<Incoming>> {
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
fn sender_for(shared: &Shared, tenant: &str) -> Option<mpsc::Sender<Incoming>> {
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

struct Outbound {
    conn: u64,
    tx: mpsc::Sender<String>,
}

#[derive(serde::Deserialize)]
struct Text {
    text: String,
}

#[derive(serde::Serialize)]
struct Reply<'a> {
    session: &'a str,
    text: &'a str,
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
            legacy: Mutex::new(Vec::new()),
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
        Some(Arc::new(TenantChannel {
            tenant: tenant.to_string(),
            shared: self.shared.clone(),
            inbound: Mutex::new(inbound),
        }))
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

#[async_trait]
impl Channel for TcpChannel {
    /// Every company at once. One engine over the whole socket is what
    /// `ns-app serve` still is (the registry that gives each company its
    /// own is plan B9), so the handle drains the companies it is told
    /// about: it adopts a receiver exactly as [`tenant_channel`] does, from
    /// the same wake stream. A process therefore uses this or
    /// `tenant_channel`, never both.
    ///
    /// Never `Closed`: `&self` is the listener, so it is alive for as long
    /// as this call is.
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        let mut held = self.legacy.lock().await;
        let mut wake = self.wake_rx.lock().await;
        loop {
            match poll_fn(|cx| poll_any(&mut held, &mut wake, cx)).await {
                Drained::Message(incoming) => return Ok(incoming),
                Drained::Adopt(tenant) => {
                    if let Some(rx) = take_parked(&self.shared, &tenant) {
                        held.push((tenant, rx));
                    }
                }
                Drained::Gone(i) => {
                    held.remove(i);
                }
                Drained::Closed => return Err(ChannelError::Closed),
            }
        }
    }

    /// Never an error: a reply that cannot be delivered is already in the
    /// engine's log, and the session's turn must not fail over a peer that
    /// left mid-turn.
    async fn send(&self, session: &SessionId, text: &str) -> Result<(), ChannelError> {
        deliver(&self.shared, session, text);
        Ok(())
    }
}

/// What one poll of the whole-socket drain found.
enum Drained {
    Message(Incoming),
    /// A company nobody is draining: take its receiver and keep it.
    Adopt(String),
    /// The receiver at this index has closed; it is of no further use.
    Gone(usize),
    /// The wake stream itself has gone, which the listener outlives.
    Closed,
}

/// Polls every adopted receiver and then the wake stream. Earlier
/// companies are preferred over later ones, which matters only when both
/// are saturated, and the queue that then waits is one company's own.
fn poll_any(
    held: &mut [(String, mpsc::Receiver<Incoming>)],
    wake: &mut mpsc::UnboundedReceiver<String>,
    cx: &mut Context<'_>,
) -> Poll<Drained> {
    for (i, (_, rx)) in held.iter_mut().enumerate() {
        match rx.poll_recv(cx) {
            Poll::Ready(Some(incoming)) => return Poll::Ready(Drained::Message(incoming)),
            Poll::Ready(None) => return Poll::Ready(Drained::Gone(i)),
            Poll::Pending => {}
        }
    }
    match wake.poll_recv(cx) {
        Poll::Ready(Some(tenant)) => Poll::Ready(Drained::Adopt(tenant)),
        Poll::Ready(None) => Poll::Ready(Drained::Closed),
        Poll::Pending => Poll::Pending,
    }
}

/// One reply to every connection holding its session, whichever company
/// asked for it.
fn deliver(shared: &Shared, session: &SessionId, text: &str) {
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

/// Writes replies as they come until the sender goes (this connection's
/// entry was released) or the socket refuses one.
async fn write_replies(mut w: OwnedWriteHalf, session: SessionId, mut rx: mpsc::Receiver<String>) {
    while let Some(text) = rx.recv().await {
        let mut line = serde_json::to_string(&Reply {
            session: &session.0,
            text: &text,
        })
        .expect("two strings serialize");
        line.push('\n');
        if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        ch.send(&session, "after the panic")
            .await
            .expect("send never fails");
        assert_eq!(rx.try_recv().ok().as_deref(), Some("after the panic"));
    }
}

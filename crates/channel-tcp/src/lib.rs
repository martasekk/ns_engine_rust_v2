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
//! §1). Each pushes into one inbound queue that `recv` drains — the
//! dispatcher keeps exactly one `recv` pending, and a full queue holds the
//! sockets back rather than dropping. `send` routes by session id to every
//! connection holding that session, each with its own bounded queue; a
//! reply for a session with no live connection, or for one that has stopped
//! reading, is logged and dropped — the engine's log already has it, and
//! that is what the log is for. One peer that stops reading therefore loses
//! its own replies and nobody else's. The connection cap is machine-wide,
//! not per socket, and the accept loop keeps an inbound sender of its own,
//! so `recv` reports `Closed` only once the accept loop itself has ended.
//!
//! # Deliberately not here
//!
//! No TLS, and loopback by default: a non-loopback bind is refused unless
//! `allow_remote` says it was meant, and a resolver that proves nothing is
//! refused off loopback whatever `allow_remote` says. Who a connection is
//! belongs to `ns-identity`; this crate knows only that something turned a
//! hello into a session id.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use nsidentity::{Hello, IdentityResolver, SharedTokenResolver};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, Mutex};

/// Messages waiting for the dispatcher, across every connection. Full means
/// the connection tasks stop reading their sockets until it drains —
/// backpressure, never a drop.
const INBOUND_DEPTH: usize = 64;
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

pub struct TcpChannel {
    local_addr: SocketAddr,
    /// One `recv` at a time, as the dispatcher guarantees; the lock is what
    /// lets the trait's `&self` hold.
    inbound: Mutex<mpsc::Receiver<Incoming>>,
    shared: Arc<Shared>,
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
        let (tx, rx) = mpsc::channel(INBOUND_DEPTH);
        let shared = Arc::new(Shared {
            resolver: auth,
            hello_timeout,
            max_connections,
            live: AtomicUsize::new(0),
            next_conn: AtomicU64::new(0),
            outbound: StdMutex::new(HashMap::new()),
        });
        tokio::spawn(accept_loop(listener, shared.clone(), tx));
        Ok(Arc::new(Self {
            local_addr,
            inbound: Mutex::new(rx),
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
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        self.inbound
            .lock()
            .await
            .recv()
            .await
            .ok_or(ChannelError::Closed)
    }

    /// Never an error: a reply that cannot be delivered is already in the
    /// engine's log, and the session's turn must not fail over a peer that
    /// left mid-turn.
    async fn send(&self, session: &SessionId, text: &str) -> Result<(), ChannelError> {
        // Cloned out from under the lock: the lock is a std one, and what
        // follows is per connection.
        let holders: Vec<mpsc::Sender<String>> = self
            .shared
            .outbound
            .lock()
            .expect("outbound map")
            .get(session)
            .map(|v| v.iter().map(|o| o.tx.clone()).collect())
            .unwrap_or_default();
        if holders.is_empty() {
            eprintln!(
                "tcp: no connection holds session {}; the reply is in the log only",
                session.0
            );
            return Ok(());
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
        Ok(())
    }
}

/// Accept until the listener fails or the channel is dropped, one task per
/// connection. The cap is counted here, before the task exists, so it is
/// the count of connections and not of sessions.
async fn accept_loop(listener: TcpListener, shared: Arc<Shared>, inbound: mpsc::Sender<Incoming>) {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            // The channel was dropped: nothing would read what a new
            // connection sends, and holding the port would be a lie.
            _ = inbound.closed() => return,
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
        let inbound = inbound.clone();
        tokio::spawn(async move {
            connection(&shared, inbound, stream, peer, conn).await;
            shared.live.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// One connection: the hello, then lines in until EOF. Lives exactly as
/// long as its writer does — which ends when its own entry is released or
/// the socket stops taking replies — and another connection joining the
/// same session touches neither.
async fn connection(
    shared: &Shared,
    inbound: mpsc::Sender<Incoming>,
    stream: TcpStream,
    peer: SocketAddr,
    conn: u64,
) {
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
    let session = match shared.resolver.resolve(hello).await {
        Ok(identity) => identity.session,
        Err(denied) => {
            eprintln!("tcp: {peer}: refused ({denied})");
            return;
        }
    };

    let (tx, rx) = mpsc::channel(OUTBOUND_DEPTH);
    // Joining, not taking over: the entries already there keep their
    // senders, and so their connections.
    let holders = {
        let mut map = shared.outbound.lock().expect("outbound map");
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
        let read = tokio::select! {
            read = reader.read_line(&mut line) => read,
            // The socket refused a reply, or this connection's entry went:
            // it is over, and a half-read line goes with it.
            _ = &mut writer => break,
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
        if inbound.send(incoming).await.is_err() {
            // The channel is gone; nothing would read this.
            break;
        }
    }

    // Release this connection's own entry, and the session itself only once
    // no window is left holding it.
    let mut map = shared.outbound.lock().expect("outbound map");
    if let Some(holders) = map.get_mut(&session) {
        holders.retain(|o| o.conn != conn);
        if holders.is_empty() {
            map.remove(&session);
        }
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

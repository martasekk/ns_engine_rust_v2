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
//! - The client's first line: `{"token":"…","session":"…"}`. A wrong or
//!   missing token, an empty session, or a line that is not that object
//!   closes the connection with nothing sent back — a caller that failed the
//!   hello learns nothing about why. A valid hello *claims* the session:
//!   replies for that session id go to this connection, and a later
//!   connection claiming the same id takes over — most recent wins, and the
//!   displaced connection is closed so its client knows.
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
//! sockets back rather than dropping. `send` routes by session id to the
//! connection holding that session; a reply for a session with no live
//! connection, or one that has stopped reading, is logged and dropped — the
//! engine's log already has it, and that is what the log is for. The
//! connection cap is machine-wide, not per socket, and the accept loop keeps
//! an inbound sender of its own, so `recv` reports `Closed` only once the
//! accept loop itself has ended.
//!
//! # Deliberately not here
//!
//! No authentication beyond the one shared token, no TLS, and loopback by
//! default: a non-loopback bind is refused unless `allow_remote` says it was
//! meant. Multi-user auth is a v1 non-goal (findings §6, last bullet) —
//! identity is the connection.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

/// Why `bind` refused to start. Each of the first two is a mistake that
/// would otherwise fail silently and permanently: a server with no token
/// answers anyone who finds the port, and one bound to `0.0.0.0` by accident
/// is the whole conversation on the network.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("refusing to start with an empty token: there is no unauthenticated mode")]
    EmptyToken,
    #[error("refusing to serve on {0}: not a loopback address — set allow_remote to mean it")]
    NotLoopback(SocketAddr),
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
    token: String,
    max_connections: usize,
    /// Live connections, machine-wide.
    live: AtomicUsize,
    /// Numbers connections, so one that hangs up releases its session only
    /// if it still holds it.
    next_conn: AtomicU64,
    /// Which connection holds each session, and the way to write to it.
    outbound: StdMutex<HashMap<SessionId, Outbound>>,
}

struct Outbound {
    conn: u64,
    tx: mpsc::Sender<String>,
}

#[derive(serde::Deserialize)]
struct Hello {
    token: String,
    session: String,
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
    /// and not in the accept loop, then spawns the accept loop. Refuses an
    /// empty token, and a non-loopback address unless `allow_remote`.
    pub async fn bind(
        listen: &str,
        token: String,
        max_connections: usize,
        allow_remote: bool,
    ) -> Result<Arc<TcpChannel>, BindError> {
        if token.is_empty() {
            return Err(BindError::EmptyToken);
        }
        let listener = TcpListener::bind(listen).await?;
        let local_addr = listener.local_addr()?;
        if !local_addr.ip().is_loopback() && !allow_remote {
            return Err(BindError::NotLoopback(local_addr));
        }
        let (tx, rx) = mpsc::channel(INBOUND_DEPTH);
        let shared = Arc::new(Shared {
            token,
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

    /// Where the listener actually is — the port, when `listen` said `:0`.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
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
        let tx = self
            .shared
            .outbound
            .lock()
            .expect("outbound map")
            .get(session)
            .map(|o| o.tx.clone());
        let Some(tx) = tx else {
            eprintln!(
                "tcp: no connection holds session {}; the reply is in the log only",
                session.0
            );
            return Ok(());
        };
        if let Err(e) = tx.try_send(text.to_string()) {
            let why = match e {
                TrySendError::Full(_) => "has stopped reading",
                TrySendError::Closed(_) => "has gone",
            };
            eprintln!(
                "tcp: the connection holding session {} {why}; the reply is in the log only",
                session.0
            );
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
/// long as its writer does — which ends when the session is taken over by
/// a newer connection or the socket stops taking replies — so a displaced
/// client sees EOF rather than silence.
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
    // The hello. Anything short of a valid one closes the connection with
    // nothing sent: a caller that failed it learns nothing about why.
    match reader.read_line(&mut line).await {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    let session = match serde_json::from_str::<Hello>(line.trim()) {
        Ok(h) if constant_time_eq(&h.token, &shared.token) && !h.session.is_empty() => {
            SessionId(h.session)
        }
        Ok(_) => {
            eprintln!("tcp: {peer}: refused (wrong token or empty session)");
            return;
        }
        Err(_) => {
            eprintln!("tcp: {peer}: refused (malformed hello)");
            return;
        }
    };

    let (tx, rx) = mpsc::channel(OUTBOUND_DEPTH);
    // The displaced entry, if any, is dropped here and not held: its sender
    // is what keeps the older connection's writer — and so the older
    // connection — alive.
    let displaced = shared
        .outbound
        .lock()
        .expect("outbound map")
        .insert(session.clone(), Outbound { conn, tx })
        .is_some();
    if displaced {
        eprintln!("tcp: {peer} takes over session {}", session.0);
    }
    // Writing is its own task, so a reply never waits behind a read.
    let mut writer = tokio::spawn(write_replies(w, session.clone(), rx));

    loop {
        line.clear();
        let read = tokio::select! {
            read = reader.read_line(&mut line) => read,
            // Displaced, or the socket refused a reply: this connection is
            // over, and a half-read line goes with it.
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

    // Release the session — unless a newer connection holds it now.
    let mut map = shared.outbound.lock().expect("outbound map");
    if map.get(&session).is_some_and(|o| o.conn == conn) {
        map.remove(&session);
    }
}

/// Writes replies as they come until the sender goes (the session was
/// released or taken over) or the socket refuses one.
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

/// Compares every byte regardless of where they first differ (as the
/// pointer agent does for its token).
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

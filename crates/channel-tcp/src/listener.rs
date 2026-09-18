//! The socket: binding it, accepting on it, and one connection's life.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nschannel_hub::{Hub, Ingress, TenantChannel};
use nscore::Incoming;
use nsidentity::{Hello, IdentityResolver, SharedTokenResolver};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

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

/// What one accept loop and its connections share: who a hello is, and how
/// many connections this socket will hold. Everything a *company* has —
/// its queue, its windows — is the hub's, not this.
struct Listen {
    /// Turns a hello into an identity, or refuses it. The only thing here
    /// that knows what a credential is.
    resolver: Arc<dyn IdentityResolver<Hello>>,
    hello_timeout: Duration,
    max_connections: usize,
    /// Live connections on this socket.
    live: AtomicUsize,
    hub: Arc<Hub>,
}

/// The listener itself: one socket, and the hub behind it. Dropping it
/// closes this way in, and — when it is the last one open — shuts the hub
/// down, so every [`TenantChannel`] reports closed once its own queue has
/// drained.
pub struct TcpChannel {
    local_addr: SocketAddr,
    /// This socket's registration with the hub. Dropped with the listener,
    /// which is how a shard learns a way in has closed.
    ingress: Ingress,
}

impl TcpChannel {
    /// Binds `listen` before returning, so a bad address fails at startup
    /// and not in the accept loop, then spawns the accept loop. `auth` says
    /// who each hello is; a non-loopback address is refused unless
    /// `allow_remote`, and a resolver that proves nothing is refused off
    /// loopback whatever `allow_remote` says.
    ///
    /// The hub is this listener's own, which is the single-way-in shape
    /// every existing caller and test has. A shard that also serves HTTP
    /// passes one in with [`bind_on`](Self::bind_on).
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
        Self::bind_on(
            Hub::new(),
            listen,
            auth,
            max_connections,
            allow_remote,
            hello_timeout,
        )
        .await
    }

    /// [`bind_with`](Self::bind_with) onto a hub that already exists, so
    /// this socket is one way in among several: a message that arrives here
    /// and one that arrives over HTTP join the same company's queue, and a
    /// reply reaches whichever windows hold the session, however each of
    /// them connected.
    pub async fn bind_on(
        hub: Arc<Hub>,
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
        let ingress = hub.ingress();
        let listen = Arc::new(Listen {
            resolver: auth,
            hello_timeout,
            max_connections,
            live: AtomicUsize::new(0),
            hub,
        });
        tokio::spawn(accept_loop(listener, listen));
        Ok(Arc::new(Self {
            local_addr,
            ingress,
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

    /// The hub this socket feeds. A shard hands it to the registry and to
    /// every other way in it opens.
    pub fn hub(&self) -> &Arc<Hub> {
        self.ingress.hub()
    }

    /// One company's side of the hub. Delegated, and kept here because it is
    /// what every caller of this crate already asks a listener for.
    pub fn tenant_channel(&self, tenant: &str) -> Option<Arc<TenantChannel>> {
        self.hub().tenant_channel(tenant)
    }

    /// The next company with messages and no engine draining them.
    pub async fn next_active_tenant(&self) -> Option<String> {
        self.hub().next_active_tenant().await
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

/// Accept until the listener fails or the hub shuts down, one task per
/// connection. The cap is counted here, before the task exists, so it is
/// the count of connections and not of sessions.
async fn accept_loop(listener: TcpListener, listen: Arc<Listen>) {
    // Numbers nothing but the log line; the hub numbers the sinks.
    let peers = AtomicU64::new(0);
    loop {
        // Registered on the notify before the flag is read, so a shutdown
        // on either side of this line is seen: already up, and the flag
        // says so; set afterwards, and the notify reaches a waiter that is
        // already there.
        let mut shutdown = Box::pin(listen.hub.notified());
        shutdown.as_mut().enable();
        if listen.hub.is_closed() {
            return;
        }
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            // The hub is shutting down: nothing would read what a new
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

        let live = listen.live.fetch_add(1, Ordering::SeqCst) + 1;
        if live > listen.max_connections {
            listen.live.fetch_sub(1, Ordering::SeqCst);
            eprintln!(
                "tcp: refusing {peer}: {} connections already",
                listen.max_connections
            );
            drop(stream);
            continue;
        }
        peers.fetch_add(1, Ordering::SeqCst);
        let listen = listen.clone();
        tokio::spawn(async move {
            connection(&listen, stream, peer).await;
            listen.live.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// One connection: the hello, then lines in until EOF. Lives exactly as
/// long as its writer does — which ends when its own sink is released or
/// the socket stops taking replies — and another connection joining the
/// same session touches neither.
async fn connection(listen: &Listen, stream: TcpStream, peer: SocketAddr) {
    let (r, w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    // The hello, under a deadline and a length bound: a peer that says
    // nothing must not hold a connection slot, and one that says too much
    // must not be buffered while it does it. Anything short of a valid hello
    // closes the connection with nothing sent — a caller that failed it
    // learns nothing about why, and the log says which refusal it was.
    let mut bounded = (&mut reader).take(HELLO_MAX);
    let read = match tokio::time::timeout(listen.hello_timeout, bounded.read_line(&mut line)).await
    {
        Ok(read) => read,
        Err(_) => {
            eprintln!(
                "tcp: {peer}: refused (no hello within {:?})",
                listen.hello_timeout
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
    let (tenant, session) = match listen.resolver.resolve(hello).await {
        Ok(identity) => (identity.tenant, identity.session),
        Err(denied) => {
            eprintln!("tcp: {peer}: refused ({denied})");
            return;
        }
    };

    // Joining, not taking over: the windows already there keep their sinks.
    let (held, rx) = listen.hub.attach(&session);
    let holders = held.holders();
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
        let mut shutdown = Box::pin(listen.hub.notified());
        shutdown.as_mut().enable();
        if listen.hub.is_closed() {
            break;
        }
        let read = tokio::select! {
            read = reader.read_line(&mut line) => read,
            // The socket refused a reply, or this connection's sink went:
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
        let Some(tx) = listen.hub.sender_for(&tenant) else {
            // The hub has gone; nothing would read this.
            break;
        };
        if !listen.hub.hand_over(&tx, incoming).await {
            break;
        }
    }

    // Dropping the handle releases this connection's own sink, and the
    // session itself only once no window is left holding it.
    drop(held);
}

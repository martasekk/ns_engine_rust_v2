//! The socket, the routes, and what every caller is allowed before it is
//! anybody.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nschannel_hub::{Hub, Ingress};
use nsidentity::{Hello, IdentityResolver};
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};

use crate::http::{read_request, Response};

use crate::{chat, hooks, messages};

/// `GET`, upgraded: a window that holds a session for as long as it is open.
pub const CHAT_PATH: &str = "/chat";
/// `POST`: one message, one reply, for a caller that holds nothing open.
pub const MESSAGES_PATH: &str = "/v1/messages";
/// `POST` (and the platforms' `GET` verification): everything under here is
/// a platform's webhook, named by the segment after it.
pub const HOOKS_PREFIX: &str = "/hooks/";
/// `GET`: is this shard up. Says nothing else — not which companies it
/// hosts, not how many callers it has.
pub const HEALTH_PATH: &str = "/healthz";

/// Which browsers may call the JSON route.
///
/// A WebSocket is not subject to the same-origin policy and a bearer token
/// is not a cookie, so this is not what keeps a caller out — the credential
/// is. It is what stops a page on another origin from quietly spending a
/// visitor's token from their browser, and it is checked on the upgrade too
/// for the same reason.
#[derive(Debug, Clone)]
pub enum Origins {
    /// Any origin, which is the right answer for a widget embedded on
    /// customer sites whose domains this shard does not know.
    Any,
    /// These exactly, matched whole. Empty means no cross-origin caller at
    /// all, which is the default and the right answer for a first-party
    /// page served beside the shard.
    These(Vec<String>),
}

impl Origins {
    fn allows(&self, origin: &str) -> bool {
        match self {
            Origins::Any => true,
            Origins::These(list) => list.iter().any(|o| o == origin),
        }
    }
}

/// What the HTTP way in needs to know, none of it about any one company.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub listen: String,
    /// A non-loopback bind is refused unless this says it was meant — the
    /// same rule, for the same reason, as `ns-channel-tcp`.
    pub allow_remote: bool,
    pub max_connections: usize,
    /// How long a socket has to send its hello, upgraded or not.
    pub hello_timeout: Duration,
    /// How long [`MESSAGES_PATH`] waits for the engine's reply before it
    /// answers 504. A turn takes seconds; a model having a bad day takes
    /// longer than a caller will hold a request open.
    pub reply_timeout: Duration,
    /// The most a request body may be. A webhook batch is the large case.
    pub max_body: usize,
    pub origins: Origins,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8787".into(),
            allow_remote: false,
            max_connections: 256,
            hello_timeout: Duration::from_secs(5),
            reply_timeout: Duration::from_secs(120),
            max_body: 256 * 1024,
            origins: Origins::These(Vec::new()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("refusing to serve on {0}: not a loopback address — set allow_remote to mean it")]
    NotLoopback(SocketAddr),
    #[error("refusing to serve on {addr} with {auth}: it proves nothing, so it is loopback only")]
    SharedAuthOffLoopback { addr: SocketAddr, auth: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Everything one connection needs, and nothing that belongs to a company.
pub(crate) struct Serve {
    pub(crate) hub: Arc<Hub>,
    /// Who a chat hello is. The webhook routes do not use it: a platform
    /// vouches for its own users, and its adapter is the resolver there.
    pub(crate) resolver: Arc<dyn IdentityResolver<Hello>>,
    pub(crate) platforms: HashMap<String, Arc<crate::PlatformEndpoint>>,
    pub(crate) cfg: HttpConfig,
    live: AtomicUsize,
}

/// The HTTP way in: a browser's chat window, a desktop app's request, and
/// every platform webhook this shard answers.
pub struct HttpChannel {
    local_addr: SocketAddr,
    /// This endpoint's registration with the hub. Dropped with the channel,
    /// which is how a shard learns a way in has closed.
    ingress: Ingress,
}

impl HttpChannel {
    /// Binds before returning, so a bad address fails at startup rather than
    /// in the accept loop, then serves until the hub shuts down or this
    /// handle is dropped.
    ///
    /// The hub is passed in rather than made here: this is a *second* way in
    /// on purpose, and a message that arrives over a WebSocket joins the
    /// same company queue as one that arrived over TCP.
    pub async fn bind_on(
        hub: Arc<Hub>,
        cfg: HttpConfig,
        resolver: Arc<dyn IdentityResolver<Hello>>,
        platforms: Vec<Arc<crate::PlatformEndpoint>>,
    ) -> Result<Arc<HttpChannel>, BindError> {
        for addr in tokio::net::lookup_host(&cfg.listen).await? {
            check_address(addr, resolver.as_ref(), cfg.allow_remote)?;
        }
        let listener = TcpListener::bind(&cfg.listen).await?;
        let local_addr = listener.local_addr()?;
        check_address(local_addr, resolver.as_ref(), cfg.allow_remote)?;
        let ingress = hub.ingress();
        let serve = Arc::new(Serve {
            hub,
            resolver,
            platforms: platforms
                .into_iter()
                .map(|p| (p.name().to_string(), p))
                .collect(),
            cfg,
            live: AtomicUsize::new(0),
        });
        tokio::spawn(accept_loop(listener, serve));
        Ok(Arc::new(HttpChannel {
            local_addr,
            ingress,
        }))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn hub(&self) -> &Arc<Hub> {
        self.ingress.hub()
    }
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

async fn accept_loop(listener: TcpListener, serve: Arc<Serve>) {
    loop {
        // Registered before the flag is read, so a shutdown on either side
        // of this line is seen — the discipline every await in this crate
        // and in `ns-channel-tcp` keeps.
        let mut shutdown = Box::pin(serve.hub.notified());
        shutdown.as_mut().enable();
        if serve.hub.is_closed() {
            return;
        }
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = &mut shutdown => return,
        };
        let (stream, peer) = match accepted {
            Ok(a) => a,
            Err(e) => {
                eprintln!("http: accept failed, no longer listening: {e}");
                return;
            }
        };
        let _ = stream.set_nodelay(true);
        let live = serve.live.fetch_add(1, Ordering::SeqCst) + 1;
        if live > serve.cfg.max_connections {
            serve.live.fetch_sub(1, Ordering::SeqCst);
            eprintln!(
                "http: refusing {peer}: {} connections already",
                serve.cfg.max_connections
            );
            drop(stream);
            continue;
        }
        let serve = serve.clone();
        tokio::spawn(async move {
            connection(&serve, stream, peer).await;
            serve.live.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// One connection: requests until the peer goes, or one upgrade and then the
/// chat window's own loop for as long as it stays open.
async fn connection(serve: &Arc<Serve>, stream: TcpStream, peer: SocketAddr) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    loop {
        // The first line is under the hello deadline for the same reason the
        // TCP hello is: a peer that connects and says nothing must not hold
        // a connection slot indefinitely.
        let read = tokio::time::timeout(
            serve.cfg.hello_timeout,
            read_request(&mut reader, serve.cfg.max_body),
        )
        .await;
        let request = match read {
            Err(_) => {
                eprintln!(
                    "http: {peer}: closed (no request within {:?})",
                    serve.cfg.hello_timeout
                );
                return;
            }
            Ok(Err(e)) => {
                eprintln!("http: {peer}: refused ({e})");
                let _ = Response::refused(e.status()).write_to(&mut w, false).await;
                return;
            }
            // A clean end between requests: the normal way a keep-alive
            // socket finishes.
            Ok(Ok(None)) => return,
            Ok(Ok(Some(request))) => request,
        };

        let origin = request.header("origin").map(str::to_string);
        if let Some(origin) = &origin {
            if !serve.cfg.origins.allows(origin) {
                eprintln!("http: {peer}: refused (origin {origin} is not allowed)");
                let _ = Response::refused(403).write_to(&mut w, false).await;
                return;
            }
        }

        // The upgrade takes the socket over and never comes back to this
        // loop: from here the peer is a chat window, not a request.
        if request.is_websocket_upgrade() {
            if request.path() != CHAT_PATH {
                let _ = Response::refused(404).write_to(&mut w, false).await;
                return;
            }
            chat::serve(serve, request, reader, w, peer).await;
            return;
        }

        let keep_alive = request.keep_alive;
        let response = route(serve, &request, peer).await;
        let response = match &origin {
            Some(origin) => with_cors(response, origin),
            None => response,
        };
        if response.write_to(&mut w, keep_alive).await.is_err() || !keep_alive {
            return;
        }
    }
}

async fn route(serve: &Arc<Serve>, request: &crate::http::Request, peer: SocketAddr) -> Response {
    match (request.method.as_str(), request.path()) {
        ("GET", HEALTH_PATH) => Response::text(200, "ok"),
        // The preflight a browser sends before the JSON route. It carries no
        // credential, so it is answered before anything is resolved.
        ("OPTIONS", _) => Response::text(204, "")
            .with_header("access-control-allow-methods", "POST, GET, OPTIONS")
            .with_header(
                "access-control-allow-headers",
                "authorization, content-type",
            )
            .with_header("access-control-max-age", "600"),
        ("POST", MESSAGES_PATH) => messages::one_message(serve, request, peer).await,
        ("GET", path) if path.starts_with(HOOKS_PREFIX) => {
            hooks::verification(serve, request, peer)
        }
        ("POST", path) if path.starts_with(HOOKS_PREFIX) => {
            hooks::delivery(serve, request, peer).await
        }
        ("GET", CHAT_PATH) => Response::refused(400),
        (_, MESSAGES_PATH) => Response::refused(405),
        _ => Response::refused(404),
    }
}

/// The two headers a browser needs to read a reply it was allowed to ask
/// for. The origin is echoed rather than starred, because a starred origin
/// cannot be paired with credentials and because echoing names exactly who
/// was let in.
fn with_cors(response: Response, origin: &str) -> Response {
    response
        .with_header("access-control-allow-origin", origin)
        .with_header("vary", "origin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_origin_list_is_no_cross_origin_caller_at_all() {
        let none = Origins::These(Vec::new());
        assert!(!none.allows("https://example.com"));
        let some = Origins::These(vec!["https://example.com".into()]);
        assert!(some.allows("https://example.com"));
        assert!(
            !some.allows("https://example.com.evil.test"),
            "matched whole, never as a prefix"
        );
        assert!(Origins::Any.allows("https://anything.test"));
    }
}

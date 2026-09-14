//! `/hooks/<platform>`: a platform delivering what its users said, and the
//! way a reply gets back to them.
//!
//! Five things happen here, in this order, and the order is the design:
//!
//! 1. **Verify, over the raw bytes.** Before anything is parsed, because a
//!    body that has been through a JSON round trip will not hash and because
//!    parsing untrusted bytes we have not authenticated is work done for an
//!    attacker.
//! 2. **Sort.** A delivery may carry a batch, and a batch may be out of
//!    order. Per-session ordering downstream is the mailbox's, but it can
//!    only preserve the order it is given (plan T6.3).
//! 3. **De-duplicate.** A retry carries the same message id, and the turn —
//!    not just the log — must run once (plan H4, [`crate::seen`]).
//! 4. **Attach the way back.** A platform session has no connection holding
//!    it, so the sink is a task that calls the platform's API. This is the
//!    one real difference from a chat window, and it is why the hub deals in
//!    sinks rather than in connections (plan T6.5).
//! 5. **Enqueue, and answer.** The platform gets its `200` in milliseconds
//!    while the turn takes seconds. A queue that is full is a `503`, which
//!    the platform will retry — and the retry will be de-duplicated if the
//!    message did land after all.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nschannel_hub::{Hub, SinkHandle};
use nscore::{Incoming, SessionId};

use crate::http::{Request, Response};
use crate::platform::{Arrival, Platform, ReplyTo, ReplyTransport};
use crate::seen::SeenIds;
use crate::server::Serve;

/// How long a delivery may wait for a full company queue before the platform
/// is told to retry. Short: a webhook handler that blocks is a webhook that
/// gets retried anyway, and a platform that gets its retries in is better
/// than one that times out.
const ENQUEUE_DEADLINE: Duration = Duration::from_secs(2);
/// How long a platform session's way back is kept after its last message. A
/// reply is normally seconds behind; this is the bound on remembering a
/// conversation nobody has continued.
const REPLY_PATH_IDLE: Duration = Duration::from_secs(60 * 60);
/// How many times a reply is retried before it is logged and given up on.
const SEND_ATTEMPTS: usize = 3;

/// One platform, as a shard serves it: the adapter, the way out, and what it
/// has already answered.
pub struct PlatformEndpoint {
    platform: Arc<dyn Platform>,
    transport: Arc<dyn ReplyTransport>,
    seen: SeenIds,
    /// One entry per conversation with a reply path open.
    open: Mutex<HashMap<SessionId, ReplyPath>>,
}

/// A session's way back to a platform: the sink the hub delivers into, and
/// the task turning what arrives there into API calls.
struct ReplyPath {
    /// Held, because dropping it is what releases the sink.
    _sink: SinkHandle,
    last: Instant,
}

impl PlatformEndpoint {
    pub fn new(
        platform: Arc<dyn Platform>,
        transport: Arc<dyn ReplyTransport>,
        seen: SeenIds,
    ) -> Arc<PlatformEndpoint> {
        Arc::new(PlatformEndpoint {
            platform,
            transport,
            seen,
            open: Mutex::new(HashMap::new()),
        })
    }

    pub fn name(&self) -> &str {
        self.platform.name()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, ReplyPath>> {
        self.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Makes sure this conversation has a way back, and notes that it is
    /// live. Idempotent: a second message on the same conversation refreshes
    /// the path rather than opening a second one, which would send every
    /// reply twice.
    fn ensure_reply_path(self: &Arc<Self>, hub: &Arc<Hub>, arrival: &Arrival) {
        let mut open = self.locked();
        // Conversations nobody has spoken on for an hour: their sinks go, so
        // a shard that has served a million customers holds paths for the
        // ones talking now.
        open.retain(|_, path| path.last.elapsed() < REPLY_PATH_IDLE);
        if let Some(path) = open.get_mut(&arrival.session) {
            path.last = Instant::now();
            return;
        }
        let (sink, replies) = hub.attach(&arrival.session);
        open.insert(
            arrival.session.clone(),
            ReplyPath {
                _sink: sink,
                last: Instant::now(),
            },
        );
        tokio::spawn(send_replies(
            self.clone(),
            arrival.session.clone(),
            arrival.reply_to.clone(),
            replies,
        ));
    }
}

/// Everything a company's engine says on this conversation, sent back to the
/// platform. The task ends when the sink is released, which is the idle
/// sweep or the shard shutting down.
async fn send_replies(
    endpoint: Arc<PlatformEndpoint>,
    session: SessionId,
    reply_to: ReplyTo,
    mut replies: tokio::sync::mpsc::Receiver<String>,
) {
    while let Some(text) = replies.recv().await {
        let mut attempt = 1;
        loop {
            match endpoint
                .platform
                .reply(endpoint.transport.as_ref(), &reply_to, &text)
                .await
            {
                Ok(()) => break,
                Err(e) if attempt < SEND_ATTEMPTS => {
                    eprintln!(
                        "http: {}: reply to session {} failed ({e}); attempt {} of {SEND_ATTEMPTS}",
                        endpoint.name(),
                        session.0,
                        attempt,
                    );
                    // Linear and short. The platform's own rate limit is the
                    // thing worth respecting here, and a reply that is
                    // minutes late is worse than one that is lost.
                    tokio::time::sleep(Duration::from_millis(250 * attempt as u64)).await;
                    attempt += 1;
                }
                Err(e) => {
                    // Logged and dropped, the way the hub drops a reply for
                    // a window that has gone: the engine's log has it.
                    eprintln!(
                        "http: {}: giving up on a reply to session {} after {SEND_ATTEMPTS} \
                         attempts ({e}); it is in the log only",
                        endpoint.name(),
                        session.0,
                    );
                    break;
                }
            }
        }
    }
}

/// `GET /hooks/<platform>`: the handshake a platform does once, before it
/// will deliver anything. It carries no credential of ours and is answered
/// from the adapter or not at all.
pub(crate) fn verification(serve: &Arc<Serve>, request: &Request, peer: SocketAddr) -> Response {
    let Some(endpoint) = endpoint_for(serve, request) else {
        return Response::refused(404);
    };
    match endpoint.platform.verification(&|key| request.query(key)) {
        Some(echo) => Response::text(200, echo),
        None => {
            eprintln!(
                "http: {peer}: refused a {} verification (no challenge, or the wrong token)",
                endpoint.name()
            );
            Response::refused(403)
        }
    }
}

/// `POST /hooks/<platform>`: one delivery, which may be a batch.
pub(crate) async fn delivery(serve: &Arc<Serve>, request: &Request, peer: SocketAddr) -> Response {
    let Some(endpoint) = endpoint_for(serve, request) else {
        return Response::refused(404);
    };
    // Over the bytes as they arrived, before anything is parsed.
    let mut arrivals = match endpoint.platform.accept(&request.headers, &request.body) {
        Ok(arrivals) => arrivals,
        Err(rejected) => {
            eprintln!(
                "http: {peer}: refused a {} delivery ({rejected})",
                endpoint.name()
            );
            // One flat refusal: which of signature, shape or account it was
            // belongs in the log and not in the answer.
            return Response::refused(403);
        }
    };
    // A batch may be out of order; per-session ordering downstream can only
    // preserve the order it is handed.
    arrivals.sort_by_key(|a| a.at);

    let mut ran = 0usize;
    for arrival in arrivals {
        if !endpoint
            .seen
            .first_time(&arrival.tenant, &arrival.message_id)
        {
            // A retry, or a message repeated inside one batch. Silently
            // right: the platform wants a 200 and nothing else.
            continue;
        }
        endpoint.ensure_reply_path(&serve.hub, &arrival);
        let Some(tx) = serve.hub.sender_for(&arrival.tenant) else {
            return Response::refused(503);
        };
        let incoming = Incoming {
            session: arrival.session.clone(),
            text: arrival.text.clone(),
        };
        let handed =
            tokio::time::timeout(ENQUEUE_DEADLINE, serve.hub.hand_over(&tx, incoming)).await;
        if !matches!(handed, Ok(true)) {
            eprintln!(
                "http: {peer}: {}'s queue for {} did not take a message within {ENQUEUE_DEADLINE:?}; \
                 asking the platform to retry",
                endpoint.name(),
                arrival.tenant,
            );
            // The platform retries, and the retry is de-duplicated if this
            // one did land after all. Better than dropping it: a customer
            // whose message vanished has no way to know.
            return Response::refused(503);
        }
        ran += 1;
    }
    // 200 now; the turn answers over the platform's API in its own time.
    Response::json(200, &serde_json::json!({ "accepted": ran }))
}

fn endpoint_for(serve: &Arc<Serve>, request: &Request) -> Option<Arc<PlatformEndpoint>> {
    let name = request
        .path()
        .strip_prefix(crate::server::HOOKS_PREFIX)?
        .trim_end_matches('/');
    serve.platforms.get(name).cloned()
}

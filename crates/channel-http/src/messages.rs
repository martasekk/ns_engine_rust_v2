//! `POST /v1/messages`: one message, one reply, nothing held open.
//!
//! The shape for a caller that cannot or does not want to hold a socket: a
//! script, a cron job, a desktop app that would rather make a request than
//! keep a connection, a back end integrating this engine behind its own UI.
//! `curl` with a bearer token is a complete client.
//!
//! It is the same conversation as the chat window's, not a second kind of
//! one: the credential resolves to the same session id, the message joins
//! the same company queue, and the reply comes out of the same hub. A caller
//! that posts here while a browser tab is open on the same session gets the
//! reply *and so does the tab*.
//!
//! What it cannot do is wait for ever. A turn takes seconds and this holds a
//! request open while it runs, so `reply_timeout` bounds it and a caller
//! that hits the bound is told 504 — the turn is still running, and its
//! answer is in the log and on any window that holds the session.

use std::net::SocketAddr;
use std::sync::Arc;

use nscore::Incoming;
use nsidentity::Hello;

use crate::http::{Request, Response};
use crate::server::Serve;
use crate::wire::PostMessage;

pub(crate) async fn one_message(
    serve: &Arc<Serve>,
    request: &Request,
    peer: SocketAddr,
) -> Response {
    let Some(token) = bearer(request) else {
        return Response::refused(401);
    };
    let Ok(body) = serde_json::from_slice::<PostMessage>(&request.body) else {
        return Response::refused(400);
    };
    if body.text.trim().is_empty() {
        return Response::refused(400);
    }
    // The same hello the socket and the WebSocket send, assembled out of a
    // header and a body rather than read off a line. One resolver, one rule
    // about who may name a session.
    let hello = Hello {
        token,
        session: body.session,
    };
    let (tenant, session) = match serve.resolver.resolve(hello).await {
        Ok(identity) => (identity.tenant, identity.session),
        Err(denied) => {
            eprintln!("http: {peer}: refused ({denied})");
            return Response::refused(401);
        }
    };

    // Attached *before* the message is enqueued: a turn that answers
    // quickly must not answer into a session nobody is holding yet.
    let (_held, mut replies) = serve.hub.attach(&session);
    let Some(tx) = serve.hub.sender_for(&tenant) else {
        // The shard is shutting down. Retryable, and said so.
        return Response::refused(503);
    };
    let incoming = Incoming {
        session: session.clone(),
        text: body.text,
    };
    if !serve.hub.hand_over(&tx, incoming).await {
        return Response::refused(503);
    }

    match tokio::time::timeout(serve.cfg.reply_timeout, replies.recv()).await {
        Ok(Some(text)) => Response::json(
            200,
            &serde_json::json!({ "session": session.0, "text": text }),
        ),
        // The sink was released without a reply, which can only be the hub
        // going down under us.
        Ok(None) => Response::refused(503),
        Err(_) => {
            eprintln!(
                "http: {peer}: session {} did not answer within {:?}; the turn is still running",
                session.0, serve.cfg.reply_timeout
            );
            Response::json(
                504,
                &serde_json::json!({
                    "session": session.0,
                    "error": "the turn is still running; hold a chat window for the reply",
                }),
            )
        }
    }
}

/// The bearer token, or nothing. The scheme is matched case-insensitively
/// because clients disagree about its capitalisation and none of them is
/// wrong.
fn bearer(request: &Request) -> Option<String> {
    let value = request.header("authorization")?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_auth(value: &str) -> Request {
        Request {
            method: "POST".into(),
            target: "/v1/messages".into(),
            headers: vec![("Authorization".into(), value.into())],
            body: Vec::new(),
            keep_alive: false,
        }
    }

    #[test]
    fn the_bearer_scheme_is_read_whatever_its_capitalisation() {
        assert_eq!(bearer(&with_auth("Bearer abc")).as_deref(), Some("abc"));
        assert_eq!(bearer(&with_auth("bearer abc")).as_deref(), Some("abc"));
        assert_eq!(bearer(&with_auth("BEARER abc")).as_deref(), Some("abc"));
        // Anything else is not a credential this route knows.
        assert_eq!(bearer(&with_auth("Basic abc")), None);
        assert_eq!(bearer(&with_auth("Bearer ")), None);
        assert_eq!(bearer(&with_auth("abc")), None);
    }
}

//! `GET /chat`, upgraded: one window of a conversation, open for as long as
//! it is.
//!
//! This is `ns-channel-tcp`'s connection with a different envelope, and
//! deliberately not a different protocol: the same hello, the same
//! `{"text":…}` in, the same `{"session":…,"text":…}` out. A desktop app can
//! speak either; a browser can only speak this one. Anything either of them
//! learns about the other's shape is therefore worth nothing, which is the
//! point — one client, two transports, one wire.
//!
//! Two tasks, as on the socket: this one reads, and a writer owns the write
//! half. Everything that goes out — a reply, a pong, a keepalive ping, the
//! close frame — goes through the writer, so no two frames can interleave.

use std::net::SocketAddr;
use std::sync::Arc;

use nscore::Incoming;
use nsidentity::Hello;
use tokio::io::BufReader;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::http::{Request, Response};
use crate::server::Serve;
use crate::ws::{self, close, Message};

/// How often an open window is pinged. Comfortably inside the sixty seconds
/// most proxies reap an idle connection after.
const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(25);

/// What the writing task sends, whoever asked for it.
enum Out {
    Reply(String),
    /// A keepalive this task sends itself.
    Ping,
    /// The answer to a peer's ping, carrying its payload back.
    Pong(Vec<u8>),
    Close(u16),
}

/// One chat window, from the upgrade to the close.
pub(crate) async fn serve(
    serve: &Arc<Serve>,
    request: Request,
    mut reader: BufReader<OwnedReadHalf>,
    mut w: OwnedWriteHalf,
    peer: SocketAddr,
) {
    let Some(key) = request.header("sec-websocket-key") else {
        let _ = Response::refused(400).write_to(&mut w, false).await;
        return;
    };
    // Version 13 is the only one RFC 6455 defines, and the only one whose
    // framing this crate implements.
    if request.header("sec-websocket-version") != Some("13") {
        let _ = Response::refused(400)
            .with_header("sec-websocket-version", "13")
            .write_to(&mut w, false)
            .await;
        return;
    }
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\
         sec-websocket-accept: {}\r\n\r\n",
        ws::accept_key(key)
    );
    use tokio::io::AsyncWriteExt as _;
    if w.write_all(handshake.as_bytes()).await.is_err() || w.flush().await.is_err() {
        return;
    }

    // The hello is the first text frame, under the same deadline the socket
    // puts on its first line: a peer that upgrades and then says nothing
    // must not hold a connection slot for ever.
    let hello =
        match tokio::time::timeout(serve.cfg.hello_timeout, ws::read_message(&mut reader)).await {
            Err(_) => {
                eprintln!(
                    "http: {peer}: closed (no hello within {:?})",
                    serve.cfg.hello_timeout
                );
                let _ = ws::write_close(&mut w, close::REFUSED).await;
                return;
            }
            Ok(Ok(Some(Message::Text(text)))) => text,
            Ok(Ok(_)) | Ok(Err(_)) => {
                let _ = ws::write_close(&mut w, close::PROTOCOL).await;
                return;
            }
        };
    let hello = match serde_json::from_str::<Hello>(&hello) {
        Ok(hello) => hello,
        Err(_) => {
            eprintln!("http: {peer}: refused (malformed hello)");
            let _ = ws::write_close(&mut w, close::REFUSED).await;
            return;
        }
    };
    // The session is the resolver's to say, never the client's to claim, and
    // the company is the resolver's too. One flat refusal whatever was
    // wrong: which it was goes in the log and not to the caller.
    let (tenant, session) = match serve.resolver.resolve(hello).await {
        Ok(identity) => (identity.tenant, identity.session),
        Err(denied) => {
            eprintln!("http: {peer}: refused ({denied})");
            let _ = ws::write_close(&mut w, close::REFUSED).await;
            return;
        }
    };

    // Joining, not taking over: every window on this session hears every
    // reply, and this one leaving takes only its own sink.
    let (held, replies) = serve.hub.attach(&session);
    let holders = held.holders();
    if holders > 1 {
        eprintln!(
            "http: {peer} joins session {} ({holders} windows)",
            session.0
        );
    }
    let (control, control_rx) = mpsc::channel::<Out>(8);
    let mut writer = tokio::spawn(write_out(w, session.clone(), replies, control_rx));

    loop {
        let mut shutdown = Box::pin(serve.hub.notified());
        shutdown.as_mut().enable();
        if serve.hub.is_closed() {
            let _ = control.try_send(Out::Close(close::GOING_AWAY));
            break;
        }
        let message = tokio::select! {
            message = ws::read_message(&mut reader) => message,
            // The writer stopped: the socket refused a frame, or this
            // window's sink was released.
            _ = &mut writer => break,
            _ = &mut shutdown => {
                let _ = control.try_send(Out::Close(close::GOING_AWAY));
                break;
            }
        };
        let message = match message {
            Ok(Some(message)) => message,
            // The peer went, cleanly or otherwise.
            Ok(None) => break,
            Err(e) => {
                eprintln!("http: {peer}: closing ({e})");
                let code = match e {
                    ws::WsError::MessageTooLarge | ws::WsError::FrameTooLarge(_) => close::TOO_BIG,
                    _ => close::PROTOCOL,
                };
                let _ = control.try_send(Out::Close(code));
                break;
            }
        };
        let text = match message {
            Message::Text(text) => text,
            Message::Ping(payload) => {
                let _ = control.try_send(Out::Pong(payload));
                continue;
            }
            Message::Pong => continue,
            Message::Close => {
                let _ = control.try_send(Out::Close(close::NORMAL));
                break;
            }
        };
        let text = match serde_json::from_str::<crate::wire::Text>(text.trim()) {
            Ok(t) => t.text,
            Err(e) => {
                // Ignored rather than fatal, exactly as on the socket: a
                // client that sends one bad frame keeps its conversation.
                eprintln!("http: {peer}: ignoring a malformed frame ({e})");
                continue;
            }
        };
        let incoming = Incoming {
            session: session.clone(),
            text,
        };
        // Looked up per message, so this waits on the queue that company has
        // at this moment and holds up nobody else's.
        let Some(tx) = serve.hub.sender_for(&tenant) else {
            break;
        };
        if !serve.hub.hand_over(&tx, incoming).await {
            break;
        }
    }

    // Releasing the sink is what ends the writer, which is what closes the
    // socket; the session's entry goes only once no window is left.
    drop(held);
}

/// The one task that writes to this socket: replies from the hub, pongs and
/// closes from the reader, and nothing from anywhere else.
async fn write_out(
    mut w: OwnedWriteHalf,
    session: nscore::SessionId,
    mut replies: mpsc::Receiver<String>,
    mut control: mpsc::Receiver<Out>,
) {
    // A chat window is idle between turns, and an idle connection is what a
    // reverse proxy or a NAT reaps — usually at sixty seconds, without
    // telling either end. The ping keeps it in use and, because a write to a
    // socket nobody is on the other end of fails, is also how this task
    // learns a peer has gone without waiting for a reply to send.
    let mut heartbeat = tokio::time::interval(PING_EVERY);
    // The first tick is immediate; a window does not need pinging the moment
    // it connects.
    heartbeat.tick().await;
    loop {
        let out = tokio::select! {
            reply = replies.recv() => match reply {
                Some(text) => Out::Reply(text),
                // The sink was released: this window is over.
                None => return,
            },
            _ = heartbeat.tick() => Out::Ping,
            control = control.recv() => match control {
                Some(out) => out,
                None => return,
            },
        };
        let wrote = match out {
            Out::Reply(text) => {
                let line = serde_json::to_string(&crate::wire::Reply {
                    session: &session.0,
                    text: &text,
                })
                .expect("two strings serialize");
                ws::write_text(&mut w, &line).await
            }
            Out::Ping => ws::write_frame(&mut w, 0x9, b"").await,
            Out::Pong(payload) => ws::write_frame(&mut w, 0xa, &payload).await,
            Out::Close(code) => {
                let _ = ws::write_close(&mut w, code).await;
                return;
            }
        };
        if wrote.is_err() {
            return;
        }
    }
}

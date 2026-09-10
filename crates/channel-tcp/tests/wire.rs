//! The wire of `nschannel_tcp`, driven by raw `TcpStream` clients
//! (multi-conversation plan Phase 3, S3.3).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nschannel_tcp::{BindError, TcpChannel};
use nscore::{Channel, Incoming, SessionId};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Long enough for a loopback round trip on a loaded box; a test that
/// needs it has already failed.
const T: Duration = Duration::from_secs(5);
/// How long "nothing arrives" is observed for.
const QUIET: Duration = Duration::from_millis(250);

/// A line-oriented client over one socket.
struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = timeout(T, TcpStream::connect(addr))
            .await
            .expect("connect in time")
            .expect("connect");
        let (r, w) = stream.into_split();
        Self {
            reader: BufReader::new(r),
            writer: w,
        }
    }

    async fn line(&mut self, s: &str) {
        self.writer
            .write_all(format!("{s}\n").as_bytes())
            .await
            .expect("write");
    }

    async fn hello(&mut self, token: &str, session: &str) {
        let hello = serde_json::json!({"token": token, "session": session});
        self.line(&hello.to_string()).await;
    }

    async fn say(&mut self, text: &str) {
        self.line(&serde_json::json!({"text": text}).to_string())
            .await;
    }

    /// The next line, or `None` once the server has closed the connection.
    /// A reset counts as closed: on Windows a close with bytes still unread
    /// is a reset, and the tests that expect a close have often just sent
    /// something the server never read.
    async fn read(&mut self) -> Option<String> {
        let mut line = String::new();
        match timeout(T, self.reader.read_line(&mut line))
            .await
            .expect("a line or EOF in time")
        {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        }
    }

    /// True when neither a line nor a close arrives within `QUIET`.
    async fn quiet(&mut self) -> bool {
        let mut line = String::new();
        timeout(QUIET, self.reader.read_line(&mut line))
            .await
            .is_err()
    }
}

async fn bound(max_connections: usize) -> Arc<TcpChannel> {
    TcpChannel::bind("127.0.0.1:0", "t0k".into(), max_connections, false)
        .await
        .expect("bind loopback")
}

async fn recv(ch: &TcpChannel) -> Incoming {
    timeout(T, ch.recv())
        .await
        .expect("recv in time")
        .expect("recv")
}

/// True when nothing reaches `recv` within `QUIET`.
async fn nothing_received(ch: &TcpChannel) -> bool {
    timeout(QUIET, ch.recv()).await.is_err()
}

fn sid(s: &str) -> SessionId {
    SessionId(s.into())
}

/// (a) Two clients on sessions `a` and `b`: both messages reach `recv` with
/// their session, and each reply lands on its own client only, as one
/// `{"session":…,"text":…}` line.
#[tokio::test]
async fn two_sessions_each_receive_only_their_own_replies() {
    let ch = bound(8).await;
    let mut a = Client::connect(ch.local_addr()).await;
    let mut b = Client::connect(ch.local_addr()).await;
    a.hello("t0k", "a").await;
    b.hello("t0k", "b").await;
    a.say("from a").await;
    b.say("from b").await;

    let mut got = vec![recv(&ch).await, recv(&ch).await];
    got.sort_by(|x, y| x.session.0.cmp(&y.session.0));
    assert_eq!(got[0].session, sid("a"));
    assert_eq!(got[0].text, "from a");
    assert_eq!(got[1].session, sid("b"));
    assert_eq!(got[1].text, "from b");

    ch.send(&sid("b"), "rb").await.unwrap();
    ch.send(&sid("a"), "ra").await.unwrap();
    assert_eq!(
        a.read().await.as_deref(),
        Some(r#"{"session":"a","text":"ra"}"#)
    );
    assert_eq!(
        b.read().await.as_deref(),
        Some(r#"{"session":"b","text":"rb"}"#)
    );
    assert!(a.quiet().await, "b's reply never reached a");
    assert!(b.quiet().await, "a's reply never reached b");
}

/// (b) A wrong token, and a hello that is not a hello: the connection is
/// closed with nothing sent, and nothing it said reaches `recv`.
#[tokio::test]
async fn a_wrong_token_is_closed_before_any_message() {
    let ch = bound(8).await;
    let mut c = Client::connect(ch.local_addr()).await;
    c.hello("nope", "a").await;
    c.say("must never arrive").await;
    assert_eq!(c.read().await, None, "closed, nothing sent");
    assert!(nothing_received(&ch).await);

    let mut c = Client::connect(ch.local_addr()).await;
    c.line("this is not a hello").await;
    assert_eq!(c.read().await, None, "a malformed hello is closed too");
    assert!(nothing_received(&ch).await);

    let mut c = Client::connect(ch.local_addr()).await;
    c.hello("t0k", "").await;
    assert_eq!(c.read().await, None, "an empty session id is refused");
    assert!(nothing_received(&ch).await);
}

/// (c) With `max_connections = 2` the third connection is closed at once,
/// the first two keep working, and a slot comes back once a connection
/// leaves.
#[tokio::test]
async fn the_connection_past_the_cap_is_closed_and_the_others_keep_working() {
    let ch = bound(2).await;
    let addr = ch.local_addr();
    let mut one = Client::connect(addr).await;
    let mut two = Client::connect(addr).await;
    one.hello("t0k", "one").await;
    two.hello("t0k", "two").await;
    one.say("1").await;
    two.say("2").await;
    // Both are live: their messages came through.
    recv(&ch).await;
    recv(&ch).await;

    let mut three = Client::connect(addr).await;
    assert_eq!(
        three.read().await,
        None,
        "the third connection is closed at once"
    );

    one.say("still here").await;
    assert_eq!(recv(&ch).await.text, "still here");
    ch.send(&sid("two"), "yes").await.unwrap();
    assert_eq!(
        two.read().await.as_deref(),
        Some(r#"{"session":"two","text":"yes"}"#)
    );

    // `one` leaves; its slot is released when the server notices, so a
    // newcomer may need a moment.
    drop(one);
    let mut admitted = false;
    for _ in 0..20 {
        let mut four = Client::connect(addr).await;
        four.hello("t0k", "four").await;
        four.say("4").await;
        if let Ok(Ok(incoming)) = timeout(QUIET, ch.recv()).await {
            assert_eq!(incoming.session, sid("four"));
            admitted = true;
            break;
        }
    }
    assert!(admitted, "a slot came back after a connection left");
}

/// (d) A reply for a session whose connection has gone — or that no
/// connection ever claimed — is `Ok(())` and panics nothing: the log has it.
#[tokio::test]
async fn a_reply_to_a_departed_session_is_dropped_without_panic() {
    let ch = bound(8).await;
    let mut c = Client::connect(ch.local_addr()).await;
    c.hello("t0k", "gone").await;
    c.say("hi").await;
    assert_eq!(recv(&ch).await.session, sid("gone"));
    drop(c);
    // Let the connection task see the EOF and release the session.
    tokio::time::sleep(Duration::from_millis(100)).await;
    ch.send(&sid("gone"), "too late").await.unwrap();
    ch.send(&sid("never"), "nobody").await.unwrap();
}

/// (e) `bind` refuses an empty token and a non-loopback address unless
/// `allow_remote`; an io error (the port is taken) is reported as one.
#[tokio::test]
async fn bind_refuses_an_empty_token_and_a_non_loopback_address_unless_allowed() {
    let err = TcpChannel::bind("127.0.0.1:0", String::new(), 2, false)
        .await
        .err()
        .expect("an empty token is refused");
    assert!(matches!(err, BindError::EmptyToken), "{err}");

    let err = TcpChannel::bind("0.0.0.0:0", "t0k".into(), 2, false)
        .await
        .err()
        .expect("a non-loopback bind is refused by default");
    assert!(matches!(err, BindError::NotLoopback(_)), "{err}");

    let ch = TcpChannel::bind("0.0.0.0:0", "t0k".into(), 2, true)
        .await
        .expect("allow_remote binds it");
    assert!(!ch.local_addr().ip().is_loopback());

    let held = bound(2).await;
    let err = TcpChannel::bind(&held.local_addr().to_string(), "t0k".into(), 2, false)
        .await
        .err()
        .expect("a port already bound is an error at bind time");
    assert!(matches!(err, BindError::Io(_)), "{err}");
}

/// A later connection claiming a session takes it over: replies go to the
/// newcomer, and the displaced connection is closed so its client knows.
#[tokio::test]
async fn a_later_connection_claiming_the_session_takes_it_over() {
    let ch = bound(8).await;
    let mut old = Client::connect(ch.local_addr()).await;
    old.hello("t0k", "s").await;
    old.say("from old").await;
    assert_eq!(recv(&ch).await.text, "from old");

    let mut new = Client::connect(ch.local_addr()).await;
    new.hello("t0k", "s").await;
    new.say("from new").await;
    assert_eq!(recv(&ch).await.text, "from new");

    ch.send(&sid("s"), "reply").await.unwrap();
    assert_eq!(
        new.read().await.as_deref(),
        Some(r#"{"session":"s","text":"reply"}"#)
    );
    assert_eq!(old.read().await, None, "the displaced connection is closed");
}

/// A line that is not `{"text":…}` is ignored and the connection stays; a
/// reply with a newline in it is still one line on the wire.
#[tokio::test]
async fn a_malformed_line_is_ignored_and_the_connection_stays() {
    let ch = bound(8).await;
    let mut c = Client::connect(ch.local_addr()).await;
    c.hello("t0k", "m").await;
    c.line(r#"{"nope": 1}"#).await;
    c.line("").await;
    c.line("plain words").await;
    c.say("after").await;
    assert_eq!(recv(&ch).await.text, "after");
    assert!(
        nothing_received(&ch).await,
        "only the well-formed line came through"
    );

    ch.send(&sid("m"), "two\nlines").await.unwrap();
    assert_eq!(
        c.read().await.as_deref(),
        Some(r#"{"session":"m","text":"two\nlines"}"#)
    );
}

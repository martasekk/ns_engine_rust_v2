//! The wire of `nschannel_tcp`, driven by raw `TcpStream` clients
//! (multi-conversation plan Phase 3, S3.3).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use nschannel_tcp::{BindError, TcpChannel, TenantChannel, HELLO_MAX, INBOUND_DEPTH};
use nscore::{Channel, ChannelError, Incoming, SessionId};
use nsidentity::{hmac_sha256, Hello, Hs256Verifier, IdentityResolver, TenantAuth};
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

/// The company every shared-token connection speaks for: one process, one
/// token, one tenant.
const LOCAL: &str = "local";

async fn bound(max_connections: usize) -> Arc<TcpChannel> {
    TcpChannel::bind_shared("127.0.0.1:0", "t0k".into(), max_connections, false)
        .await
        .expect("bind loopback")
}

/// Unix seconds the JWT tests pretend it is.
const NOW_SECS: u64 = 1_700_000_000;

fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A token this tenant's own backend would have minted.
fn token(key: &[u8], iss: &str, sub: &str) -> String {
    let header = b64u(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = b64u(
        format!(
            r#"{{"iss":"{iss}","sub":"{sub}","iat":{},"exp":{}}}"#,
            NOW_SECS - 60,
            NOW_SECS + 600
        )
        .as_bytes(),
    );
    let signed = format!("{header}.{payload}");
    format!("{signed}.{}", b64u(&hmac_sha256(key, signed.as_bytes())))
}

/// A resolver that verifies tokens, one signing key per named tenant.
fn verifier(tenants: &[(&str, &[u8])]) -> Arc<dyn IdentityResolver<Hello>> {
    let table = tenants
        .iter()
        .map(|(name, key)| {
            (
                name.to_string(),
                TenantAuth {
                    current: key.to_vec(),
                    previous: None,
                    iat_floor: 0,
                },
            )
        })
        .collect();
    Arc::new(Hs256Verifier::with_clock(table, Box::new(|| NOW_SECS)))
}

/// A listener whose clients must present a token of tenant `acme`.
async fn bound_jwt(key: &[u8]) -> Arc<TcpChannel> {
    TcpChannel::bind("127.0.0.1:0", verifier(&[("acme", key)]), 8, false)
        .await
        .expect("bind loopback")
}

/// True when nothing reaches this company's engine within `QUIET`: its
/// queue exists and is empty. A different claim from
/// [`no_tenant_woken`]'s, which is that no queue exists at all.
async fn nothing_received(ch: &TenantChannel) -> bool {
    timeout(QUIET, ch.recv()).await.is_err()
}

/// True when no company is announced within `QUIET`. The first message to
/// reach a company creates its queue and wakes once, so a silent wake
/// stream is nothing having been queued for anybody.
async fn no_tenant_woken(ch: &TcpChannel) -> bool {
    !matches!(timeout(QUIET, ch.next_active_tenant()).await, Ok(Some(_)))
}

fn sid(s: &str) -> SessionId {
    SessionId(s.into())
}

const ACME: &[u8] = b"acme-signing-key";
const BETA: &[u8] = b"beta-signing-key";

/// A listener two companies present tokens to.
async fn bound_two() -> Arc<TcpChannel> {
    TcpChannel::bind(
        "127.0.0.1:0",
        verifier(&[("acme", ACME), ("beta", BETA)]),
        8,
        false,
    )
    .await
    .expect("bind loopback")
}

/// The registry's side of a cold company: wait to be told it has messages
/// nobody is draining, then take its channel.
async fn wait_for_tenant(ch: &TcpChannel, want: &str) -> Arc<TenantChannel> {
    loop {
        let woken = timeout(T, ch.next_active_tenant())
            .await
            .expect("a wake in time")
            .expect("the wake stream outlives the listener");
        if woken == want {
            return ch
                .tenant_channel(&woken)
                .expect("a woken company has its receiver parked");
        }
    }
}

async fn tenant_recv(ch: &TenantChannel) -> Incoming {
    timeout(T, ch.recv())
        .await
        .expect("recv in time")
        .expect("recv")
}

/// (B3) A company that floods and is drained by nobody blocks its own
/// connections and nothing else: the other company round trips while the
/// flooding connection is still stuck mid-write.
#[tokio::test]
async fn one_tenants_flood_does_not_delay_another() {
    // Enough to fill acme's queue and then the socket buffers behind it,
    // so the flooding client is certainly still writing at the end.
    const FLOOD: usize = INBOUND_DEPTH * 8;
    let ch = bound_two().await;

    let mut flooding = Client::connect(ch.local_addr()).await;
    flooding.hello(&token(ACME, "acme", "u1"), "").await;
    let flood = tokio::spawn(async move {
        let bulk = "x".repeat(8 * 1024);
        for _ in 0..FLOOD {
            flooding.say(&bulk).await;
        }
        flooding
    });

    let mut beta_client = Client::connect(ch.local_addr()).await;
    beta_client.hello(&token(BETA, "beta", "u1"), "").await;
    beta_client.say("from beta").await;

    let beta = wait_for_tenant(&ch, "beta").await;
    let incoming = tenant_recv(&beta).await;
    assert_eq!(incoming.session, sid("beta/web/u1"));
    assert_eq!(incoming.text, "from beta");
    beta.send(&sid("beta/web/u1"), "for beta").await.unwrap();
    assert_eq!(
        beta_client.read().await.as_deref(),
        Some(r#"{"session":"beta/web/u1","text":"for beta"}"#),
        "beta round trips while acme is wedged"
    );

    assert!(
        !flood.is_finished(),
        "acme is still blocked on its own queue, so that is what beta overtook"
    );
    // And nothing of acme's was dropped to let beta past: its queue is
    // full of it, and drains when somebody finally takes it.
    let acme = ch.tenant_channel("acme").expect("acme's queue exists");
    for _ in 0..INBOUND_DEPTH {
        assert_eq!(tenant_recv(&acme).await.session, sid("acme/web/u1"));
    }
    flood.abort();
}

/// (B3, risk 1) A connection stuck on a company's full queue is not what
/// the shutdown waits for: dropping the listener closes it, even though
/// the engine holding that queue is draining nothing.
#[tokio::test]
async fn listener_shutdown_unblocks_a_connection_stalled_on_a_full_tenant_queue() {
    let ch = bound_jwt(ACME).await;
    let mut c = Client::connect(ch.local_addr()).await;
    c.hello(&token(ACME, "acme", "u1"), "").await;
    c.say("first").await;

    // The engine's side: it takes the queue, reads one message and then
    // stops, so everything after this piles up and stays there.
    let acme = wait_for_tenant(&ch, "acme").await;
    assert_eq!(tenant_recv(&acme).await.text, "first");

    // Enough to fill the queue and then the socket buffers behind it. The
    // client stopping mid-write is the proof that its connection is where
    // the test says it is: waiting to hand a message to a full queue, and
    // not waiting on the socket for the next one.
    let bulk = "x".repeat(8 * 1024);
    let wedged = timeout(QUIET, async {
        for _ in 0..INBOUND_DEPTH * 64 {
            c.say(&bulk).await;
        }
    })
    .await
    .is_err();
    assert!(wedged, "the connection is stalled on acme's full queue");

    drop(ch);
    assert_eq!(
        c.read().await,
        None,
        "the stalled connection was closed by the shutdown"
    );
    // And its queue closes once what it holds is gone, rather than staying
    // open on a connection that is never coming back.
    let closed = loop {
        match timeout(T, acme.recv()).await.expect("recv in time") {
            Ok(_) => continue,
            Err(e) => break e,
        }
    };
    assert!(matches!(closed, ChannelError::Closed), "{closed}");
}

/// (B3) A company with no queue yet gets one on its first message, and the
/// wake that says so is one wake however much it then says.
#[tokio::test]
async fn a_cold_tenants_first_message_wakes_the_listener_once() {
    let ch = bound_jwt(ACME).await;
    let mut c = Client::connect(ch.local_addr()).await;
    c.hello(&token(ACME, "acme", "u1"), "").await;
    for i in 0..3 {
        c.say(&format!("m{i}")).await;
    }

    let woken = timeout(T, ch.next_active_tenant())
        .await
        .expect("a wake in time")
        .expect("the wake stream outlives the listener");
    assert_eq!(woken, "acme");
    assert!(
        timeout(QUIET, ch.next_active_tenant()).await.is_err(),
        "three messages, one wake"
    );

    // The wake was not instead of the messages: all three are in the queue
    // it announced.
    let acme = ch.tenant_channel("acme").expect("its receiver is parked");
    for i in 0..3 {
        assert_eq!(tenant_recv(&acme).await.text, format!("m{i}"));
    }
}

/// (B3) Dropping the listener is the shutdown, and it is not a loss: each
/// company's channel hands over what its queue already held and only then
/// reports closed, which is what the single inbound queue did.
#[tokio::test]
async fn dropping_the_listener_closes_every_tenant_channel_after_it_drains() {
    let ch = bound_two().await;
    let mut a = Client::connect(ch.local_addr()).await;
    a.hello(&token(ACME, "acme", "u1"), "").await;
    a.say("from acme").await;
    let mut b = Client::connect(ch.local_addr()).await;
    b.hello(&token(BETA, "beta", "u1"), "").await;
    b.say("from beta").await;

    let acme = wait_for_tenant(&ch, "acme").await;
    let beta = wait_for_tenant(&ch, "beta").await;
    drop(ch);

    for (name, tenant) in [("acme", &acme), ("beta", &beta)] {
        let incoming = tenant_recv(tenant).await;
        assert_eq!(
            incoming.text,
            format!("from {name}"),
            "what was queued before the shutdown still arrives"
        );
        let err = timeout(T, tenant.recv())
            .await
            .expect("closed in time")
            .expect_err("closed once it has drained");
        assert!(matches!(err, ChannelError::Closed), "{err}");
    }
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

    let local = wait_for_tenant(&ch, LOCAL).await;
    let mut got = [tenant_recv(&local).await, tenant_recv(&local).await];
    got.sort_by(|x, y| x.session.0.cmp(&y.session.0));
    assert_eq!(got[0].session, sid("a"));
    assert_eq!(got[0].text, "from a");
    assert_eq!(got[1].session, sid("b"));
    assert_eq!(got[1].text, "from b");

    local.send(&sid("b"), "rb").await.unwrap();
    local.send(&sid("a"), "ra").await.unwrap();
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
    assert!(no_tenant_woken(&ch).await);

    let mut c = Client::connect(ch.local_addr()).await;
    c.line("this is not a hello").await;
    assert_eq!(c.read().await, None, "a malformed hello is closed too");
    assert!(no_tenant_woken(&ch).await);

    let mut c = Client::connect(ch.local_addr()).await;
    c.hello("t0k", "").await;
    assert_eq!(c.read().await, None, "an empty session id is refused");
    assert!(no_tenant_woken(&ch).await);
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
    let local = wait_for_tenant(&ch, LOCAL).await;
    tenant_recv(&local).await;
    tenant_recv(&local).await;

    let mut three = Client::connect(addr).await;
    assert_eq!(
        three.read().await,
        None,
        "the third connection is closed at once"
    );

    one.say("still here").await;
    assert_eq!(tenant_recv(&local).await.text, "still here");
    local.send(&sid("two"), "yes").await.unwrap();
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
        if let Ok(Ok(incoming)) = timeout(QUIET, local.recv()).await {
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
    let local = wait_for_tenant(&ch, LOCAL).await;
    assert_eq!(tenant_recv(&local).await.session, sid("gone"));
    drop(c);
    // Let the connection task see the EOF and release the session.
    tokio::time::sleep(Duration::from_millis(100)).await;
    local.send(&sid("gone"), "too late").await.unwrap();
    local.send(&sid("never"), "nobody").await.unwrap();
}

/// (e) `bind` refuses an empty token and a non-loopback address unless
/// `allow_remote`; an io error (the port is taken) is reported as one.
#[tokio::test]
async fn bind_refuses_an_empty_token_and_a_non_loopback_address_unless_allowed() {
    let err = TcpChannel::bind_shared("127.0.0.1:0", String::new(), 2, false)
        .await
        .err()
        .expect("an empty token is refused");
    assert!(matches!(err, BindError::EmptyToken), "{err}");

    let err = TcpChannel::bind_shared("0.0.0.0:0", "t0k".into(), 2, false)
        .await
        .err()
        .expect("a non-loopback bind is refused by default");
    assert!(matches!(err, BindError::NotLoopback(_)), "{err}");

    // The shared token proves nothing, so it is refused off loopback even
    // when `allow_remote` says the address was meant.
    let err = TcpChannel::bind_shared("0.0.0.0:0", "t0k".into(), 2, true)
        .await
        .err()
        .expect("a resolver that proves nothing is loopback only");
    assert!(
        matches!(err, BindError::SharedAuthOffLoopback { .. }),
        "{err}"
    );

    // With a resolver that does prove something, `allow_remote` binds it.
    let ch = TcpChannel::bind("0.0.0.0:0", verifier(&[]), 2, true)
        .await
        .expect("allow_remote binds it");
    assert!(!ch.local_addr().ip().is_loopback());

    let held = bound(2).await;
    let err = TcpChannel::bind_shared(&held.local_addr().to_string(), "t0k".into(), 2, false)
        .await
        .err()
        .expect("a port already bound is an error at bind time");
    assert!(matches!(err, BindError::Io(_)), "{err}");
}

/// (f) Two windows of one conversation: a second connection claiming a
/// session joins it rather than displacing it, both stay live, and one
/// reply reaches both (multi-tenant plan Phase 5, T5.2).
#[tokio::test]
async fn two_connections_on_one_session_both_receive_the_reply() {
    let ch = bound(8).await;
    let mut tab = Client::connect(ch.local_addr()).await;
    tab.hello("t0k", "s").await;
    tab.say("from the tab").await;
    // The message proves the hello was processed, so this connection holds
    // its session by the time the reply goes out.
    let local = wait_for_tenant(&ch, LOCAL).await;
    assert_eq!(tenant_recv(&local).await.text, "from the tab");

    let mut phone = Client::connect(ch.local_addr()).await;
    phone.hello("t0k", "s").await;
    phone.say("from the phone").await;
    assert_eq!(tenant_recv(&local).await.text, "from the phone");

    local.send(&sid("s"), "reply").await.unwrap();
    assert_eq!(
        tab.read().await.as_deref(),
        Some(r#"{"session":"s","text":"reply"}"#),
        "the first connection was not displaced"
    );
    assert_eq!(
        phone.read().await.as_deref(),
        Some(r#"{"session":"s","text":"reply"}"#)
    );
}

/// (g) A departing connection takes only its own entry with it: the other
/// holder of the same session keeps sending and receiving.
#[tokio::test]
async fn closing_one_of_two_connections_leaves_the_other_serving() {
    let ch = bound(8).await;
    let mut leaving = Client::connect(ch.local_addr()).await;
    let mut staying = Client::connect(ch.local_addr()).await;
    leaving.hello("t0k", "s").await;
    staying.hello("t0k", "s").await;
    leaving.say("1").await;
    staying.say("2").await;
    let local = wait_for_tenant(&ch, LOCAL).await;
    tenant_recv(&local).await;
    tenant_recv(&local).await;

    drop(leaving);
    // Let the connection task see the EOF and release its own entry.
    tokio::time::sleep(Duration::from_millis(100)).await;

    staying.say("still here").await;
    assert_eq!(tenant_recv(&local).await.text, "still here");
    local.send(&sid("s"), "reply").await.unwrap();
    assert_eq!(
        staying.read().await.as_deref(),
        Some(r#"{"session":"s","text":"reply"}"#),
        "the survivor still holds the session"
    );
}

/// (h) A peer that stops reading loses its replies to the log; it must hold
/// up neither the caller of `send` nor the other connection on its session.
/// The flood is big enough to fill the socket buffers and then the stalled
/// connection's bounded outbound queue, so that peer is wedged for good;
/// the other window must still get the next reply.
#[tokio::test]
async fn a_stalled_connection_does_not_delay_the_other_holder_of_its_session() {
    const FLOOD: usize = 200;
    let ch = bound(8).await;
    let mut stalled = Client::connect(ch.local_addr()).await;
    let mut reading = Client::connect(ch.local_addr()).await;
    stalled.hello("t0k", "s").await;
    reading.hello("t0k", "s").await;
    stalled.say("1").await;
    reading.say("2").await;
    let local = wait_for_tenant(&ch, LOCAL).await;
    tenant_recv(&local).await;
    tenant_recv(&local).await;
    // `stalled` never calls `read` from here on.

    // Wedge it. The timeout is what a blocking send would trip on; the
    // bounded queue means this returns in microseconds instead.
    let bulk = "x".repeat(8 * 1024);
    timeout(T, async {
        for _ in 0..FLOOD {
            local.send(&sid("s"), &bulk).await.unwrap();
        }
    })
    .await
    .expect("send never waits on the stalled peer");

    // Let the reading window catch up on whatever of the flood it kept, so
    // its own queue has room again.
    while !reading.quiet().await {}

    // The wedged peer is still in the map, still full. The other window
    // gets this one anyway, and promptly.
    local.send(&sid("s"), "after").await.unwrap();
    assert_eq!(
        reading.read().await.as_deref(),
        Some(r#"{"session":"s","text":"after"}"#),
        "the stalled window did not cost the other one its reply"
    );
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
    let local = wait_for_tenant(&ch, LOCAL).await;
    assert_eq!(tenant_recv(&local).await.text, "after");
    assert!(
        nothing_received(&local).await,
        "only the well-formed line came through"
    );

    local.send(&sid("m"), "two\nlines").await.unwrap();
    assert_eq!(
        c.read().await.as_deref(),
        Some(r#"{"session":"m","text":"two\nlines"}"#)
    );
}

/// (A4) The session a verified client speaks on is the one its token
/// derives, not the one it asked for: naming somebody else's session in the
/// hello reaches neither their messages nor their replies.
#[tokio::test]
async fn a_client_cannot_choose_its_session_id() {
    const KEY: &[u8] = b"acme-signing-key";
    let ch = bound_jwt(KEY).await;
    let mut c = Client::connect(ch.local_addr()).await;
    c.hello(&token(KEY, "acme", "u1"), "acme/web/victim").await;
    c.say("hello").await;

    let acme = wait_for_tenant(&ch, "acme").await;
    let incoming = tenant_recv(&acme).await;
    assert_eq!(
        incoming.session,
        sid("acme/web/u1"),
        "the token's subject, not the claimed session"
    );

    // And the session it claimed is not one it holds: a reply meant for the
    // victim does not reach the impostor.
    acme.send(&sid("acme/web/victim"), "private").await.unwrap();
    assert!(c.quiet().await, "no reply for the claimed session arrived");
    acme.send(&sid("acme/web/u1"), "yours").await.unwrap();
    assert_eq!(
        c.read().await.as_deref(),
        Some(r#"{"session":"acme/web/u1","text":"yours"}"#)
    );
}

/// (A4) A hello with no session field at all is fine under a verified
/// resolver: the session is the token's to say, and the field is ignored
/// whether it is there or not.
#[tokio::test]
async fn a_jwt_hello_derives_its_session_and_ignores_the_clients_session_field() {
    const KEY: &[u8] = b"acme-signing-key";
    let ch = bound_jwt(KEY).await;

    let mut without = Client::connect(ch.local_addr()).await;
    without
        .line(&serde_json::json!({"token": token(KEY, "acme", "u1")}).to_string())
        .await;
    without.say("no session field").await;
    let acme = wait_for_tenant(&ch, "acme").await;
    assert_eq!(tenant_recv(&acme).await.session, sid("acme/web/u1"));

    // A second subject of the same tenant is a different session, derived
    // the same way.
    let mut other = Client::connect(ch.local_addr()).await;
    other.hello(&token(KEY, "acme", "u2"), "acme/web/u1").await;
    other.say("from u2").await;
    let incoming = tenant_recv(&acme).await;
    assert_eq!(incoming.session, sid("acme/web/u2"));
    assert_eq!(incoming.text, "from u2");

    acme.send(&sid("acme/web/u1"), "for u1").await.unwrap();
    assert_eq!(
        without.read().await.as_deref(),
        Some(r#"{"session":"acme/web/u1","text":"for u1"}"#)
    );
    assert!(other.quiet().await, "u2 did not receive u1's reply");
}

/// (A4) One tenant's key is not another's: a token signed by the wrong
/// tenant's key is closed, and two tenants' subjects land on session ids
/// that cannot collide.
#[tokio::test]
async fn a_token_for_one_tenant_cannot_reach_another() {
    const ACME: &[u8] = b"acme-signing-key";
    const OTHER: &[u8] = b"other-signing-key";
    let ch = TcpChannel::bind(
        "127.0.0.1:0",
        verifier(&[("acme", ACME), ("other", OTHER)]),
        8,
        false,
    )
    .await
    .expect("bind loopback");

    // Tenant `other`'s key, claiming to be tenant `acme`.
    let mut forged = Client::connect(ch.local_addr()).await;
    forged
        .hello(&token(OTHER, "acme", "u1"), "acme/web/u1")
        .await;
    forged.say("must never arrive").await;
    assert_eq!(forged.read().await, None, "closed, nothing sent");
    assert!(no_tenant_woken(&ch).await);

    // The same subject name under each tenant is two distinct sessions.
    let mut a = Client::connect(ch.local_addr()).await;
    a.hello(&token(ACME, "acme", "u1"), "").await;
    a.say("from acme").await;
    let acme_tenant = wait_for_tenant(&ch, "acme").await;
    assert_eq!(tenant_recv(&acme_tenant).await.session, sid("acme/web/u1"));

    let mut b = Client::connect(ch.local_addr()).await;
    b.hello(&token(OTHER, "other", "u1"), "").await;
    b.say("from other").await;
    let other_tenant = wait_for_tenant(&ch, "other").await;
    assert_eq!(
        tenant_recv(&other_tenant).await.session,
        sid("other/web/u1")
    );

    acme_tenant
        .send(&sid("acme/web/u1"), "for acme")
        .await
        .unwrap();
    assert_eq!(
        a.read().await.as_deref(),
        Some(r#"{"session":"acme/web/u1","text":"for acme"}"#)
    );
    assert!(b.quiet().await, "the other tenant saw nothing of it");
}

/// (A5) A connection that never says anything is dropped at the hello
/// deadline, and its slot comes back.
#[tokio::test]
async fn a_silent_connection_is_dropped_at_the_hello_deadline() {
    let ch = TcpChannel::bind_with(
        "127.0.0.1:0",
        Arc::new(nsidentity::SharedTokenResolver::new("t0k", "local")),
        1,
        false,
        Duration::from_millis(200),
    )
    .await
    .expect("bind loopback");

    let mut silent = Client::connect(ch.local_addr()).await;
    assert_eq!(
        silent.read().await,
        None,
        "the silent connection was closed at the deadline"
    );

    // The one slot it held is free again: a client that does say hello gets
    // in. The server needs a moment to notice the close.
    let mut spoke = None;
    for _ in 0..20 {
        let mut c = Client::connect(ch.local_addr()).await;
        c.hello("t0k", "s").await;
        c.say("here").await;
        // The company is cold until somebody gets in, so the wake is what
        // says one did; the message it announced is then in its queue.
        if let Ok(Some(tenant)) = timeout(QUIET, ch.next_active_tenant()).await {
            let local = ch
                .tenant_channel(&tenant)
                .expect("a woken company has its receiver parked");
            assert_eq!(tenant_recv(&local).await.session, sid("s"));
            spoke = Some(c);
            break;
        }
    }
    assert!(spoke.is_some(), "the silent peer released its slot");
}

/// (A5) A hello past `HELLO_MAX` is refused rather than buffered, while one
/// just inside it still works: the bound is where it says it is.
#[tokio::test]
async fn an_oversized_hello_is_refused_without_buffering_it() {
    let ch = bound(8).await;

    let mut huge = Client::connect(ch.local_addr()).await;
    // Twice the bound, on one line, with no newline until the very end: a
    // server that buffered the line would still be reading.
    let session = "x".repeat(HELLO_MAX as usize * 2);
    huge.hello("t0k", &session).await;
    huge.say("must never arrive").await;
    assert_eq!(huge.read().await, None, "closed, nothing sent");
    assert!(no_tenant_woken(&ch).await);

    // A hello that fits is untouched by the bound. The envelope around the
    // session id is well under a hundred bytes.
    let mut fits = Client::connect(ch.local_addr()).await;
    let session = "y".repeat(HELLO_MAX as usize - 100);
    fits.hello("t0k", &session).await;
    fits.say("arrives").await;
    let local = wait_for_tenant(&ch, LOCAL).await;
    let incoming = tenant_recv(&local).await;
    assert_eq!(incoming.session, sid(&session));
    assert_eq!(incoming.text, "arrives");
}

/// (A5) A non-loopback listen address is refused before the socket is
/// taken, proven by binding that very port afterwards.
#[tokio::test]
async fn a_non_loopback_listen_is_refused_before_the_port_is_bound() {
    // A port nothing holds: bound and released to learn its number.
    let port = {
        let probe = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .expect("probe bind");
        probe.local_addr().expect("probe addr").port()
    };
    let listen = format!("0.0.0.0:{port}");

    let err = TcpChannel::bind_shared(&listen, "t0k".into(), 2, false)
        .await
        .err()
        .expect("a non-loopback bind is refused");
    assert!(matches!(err, BindError::NotLoopback(_)), "{err}");

    // The refusal did not take the port with it.
    tokio::net::TcpListener::bind(&listen)
        .await
        .expect("the port was never bound");

    // And with `:0` the refusal names port 0, which only the address that
    // was asked for can be: a refusal after binding would name the
    // ephemeral port the OS had already handed out.
    let err = TcpChannel::bind_shared("0.0.0.0:0", "t0k".into(), 2, false)
        .await
        .err()
        .expect("a non-loopback bind is refused");
    assert!(
        matches!(err, BindError::NotLoopback(addr) if addr.port() == 0),
        "{err}"
    );
}

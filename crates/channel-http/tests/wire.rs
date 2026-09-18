//! The three ways in this crate serves, each driven over a real socket by a
//! client that knows nothing about its internals: a browser-shaped WebSocket
//! client, a `curl`-shaped request, and a platform-shaped signed delivery.
//!
//! The one test worth reading first is
//! [`a_reply_reaches_a_web_window_and_a_desktop_socket_at_once`]: a company
//! whose customer has a browser tab open and a desktop app running is one
//! conversation, not two, and the engine is not told which is which.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use nschannel_http::platform::MockReplyTransport;
use nschannel_http::whatsapp::{Account, WhatsApp};
use nschannel_http::{HttpChannel, HttpConfig, Origins, PlatformEndpoint, SeenIds};
use nschannel_hub::{Hub, TenantChannel};
use nscore::{Channel, Incoming};
use nsidentity::{hmac_sha256, Hello, Hs256Verifier, IdentityResolver, TenantAuth};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Long enough for a loopback round trip on a loaded box; a test that needs
/// it has already failed.
const T: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(250);
const NOW_SECS: u64 = 1_700_000_000;
const ACME: &[u8] = b"acme-signing-key";
const BETA: &[u8] = b"beta-signing-key";

fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A token this company's own back end would have minted.
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

fn config() -> HttpConfig {
    HttpConfig {
        listen: "127.0.0.1:0".into(),
        // Loopback in a test, and short deadlines so a broken case fails
        // fast rather than waiting out a production-sized timeout.
        hello_timeout: Duration::from_secs(2),
        reply_timeout: Duration::from_secs(2),
        ..HttpConfig::default()
    }
}

/// A shard with the HTTP way in open, and the hub behind it.
async fn bound(platforms: Vec<Arc<PlatformEndpoint>>) -> (Arc<HttpChannel>, Arc<Hub>) {
    bound_with(config(), platforms).await
}

async fn bound_with(
    cfg: HttpConfig,
    platforms: Vec<Arc<PlatformEndpoint>>,
) -> (Arc<HttpChannel>, Arc<Hub>) {
    let hub = Hub::new();
    let http = HttpChannel::bind_on(
        hub.clone(),
        cfg,
        verifier(&[("acme", ACME), ("beta", BETA)]),
        platforms,
    )
    .await
    .expect("bind loopback");
    (http, hub)
}

/// The registry's side: wait to be told a company has messages nobody is
/// draining, then take its channel.
async fn wait_for_tenant(hub: &Arc<Hub>, want: &str) -> Arc<TenantChannel> {
    loop {
        let woken = timeout(T, hub.next_active_tenant())
            .await
            .expect("a wake in time")
            .expect("the wake stream outlives the hub");
        if woken == want {
            return hub
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

// ---------------------------------------------------------------- a browser

/// A WebSocket client with only what a chat window uses: the handshake,
/// masked text frames out, server frames in.
struct WebClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// The status line the server answered the upgrade with: `101` when it
    /// upgraded, and whatever it refused with when it did not.
    status: String,
}

impl WebClient {
    /// Connects and upgrades. `origin` is what a browser would send.
    async fn open(addr: SocketAddr, origin: Option<&str>) -> WebClient {
        let stream = timeout(T, TcpStream::connect(addr))
            .await
            .expect("connect in time")
            .expect("connect");
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let key = STANDARD.encode(b"0123456789abcdef");
        let mut request = format!(
            "GET /chat HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        );
        if let Some(origin) = origin {
            request.push_str(&format!("Origin: {origin}\r\n"));
        }
        request.push_str("\r\n");
        w.write_all(request.as_bytes()).await.expect("write");

        // The status line and headers, up to the blank one.
        let mut status = String::new();
        reader.read_line(&mut status).await.expect("a status line");
        if status.starts_with("HTTP/1.1 101") {
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("a header");
                if line.trim().is_empty() {
                    break;
                }
            }
        }
        WebClient {
            reader,
            writer: w,
            status,
        }
    }

    /// The first frame, which is the hello.
    async fn hello(&mut self, token: &str) {
        self.send(&serde_json::json!({ "token": token }).to_string())
            .await;
    }

    async fn say(&mut self, text: &str) {
        self.send(&serde_json::json!({ "text": text }).to_string())
            .await;
    }

    /// One masked text frame, as every browser sends.
    async fn send(&mut self, text: &str) {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let payload: Vec<u8> = text
            .as_bytes()
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();
        let mut frame = vec![0x81];
        // A hello carrying a JWT is past the 7-bit length, so the client
        // sends the two-byte form for anything that does not fit — which is
        // also the form a browser uses for the same message.
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        frame.extend_from_slice(&payload);
        self.writer.write_all(&frame).await.expect("write");
    }

    /// The next server frame as (opcode, payload), or `None` at EOF.
    async fn frame(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut head = [0u8; 2];
        match timeout(T, self.reader.read_exact(&mut head)).await {
            Err(_) => panic!("a frame in time"),
            Ok(Err(_)) => return None,
            Ok(Ok(_)) => {}
        }
        let opcode = head[0] & 0x0f;
        let len = match head[1] & 0x7f {
            126 => {
                let mut ext = [0u8; 2];
                self.reader.read_exact(&mut ext).await.expect("a length");
                u16::from_be_bytes(ext) as usize
            }
            n => n as usize,
        };
        let mut payload = vec![0u8; len];
        self.reader
            .read_exact(&mut payload)
            .await
            .expect("a payload");
        Some((opcode, payload))
    }

    /// The next reply's text, skipping pongs.
    async fn reply(&mut self) -> Option<String> {
        loop {
            let (opcode, payload) = self.frame().await?;
            match opcode {
                0x1 => {
                    let value: serde_json::Value =
                        serde_json::from_slice(&payload).expect("a reply object");
                    return Some(value["text"].as_str().expect("a text field").to_string());
                }
                0x8 => return None,
                _ => continue,
            }
        }
    }

    /// The close code the server sent, if it closed.
    async fn close_code(&mut self) -> Option<u16> {
        let (opcode, payload) = self.frame().await?;
        (opcode == 0x8 && payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]]))
    }
}

/// A plain HTTP request over its own connection, the way `curl` makes one.
async fn request(addr: SocketAddr, raw: &str) -> (u16, String) {
    let mut stream = timeout(T, TcpStream::connect(addr))
        .await
        .expect("connect in time")
        .expect("connect");
    stream.write_all(raw.as_bytes()).await.expect("write");
    let mut response = String::new();
    timeout(T, stream.read_to_string(&mut response))
        .await
        .expect("a response in time")
        .expect("read");
    let status: u16 = response
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("a status line in {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, body)
}

fn post(path: &str, headers: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
        body.len()
    )
}

// ----------------------------------------------------------- a chat window

/// The whole of a web chat window: it upgrades, says who it is, speaks, and
/// hears the company's answer on the same socket.
#[tokio::test]
async fn a_web_chat_window_holds_a_session_and_hears_its_reply() {
    let (http, hub) = bound(Vec::new()).await;
    let mut window = WebClient::open(http.local_addr(), None).await;
    window.hello(&token(ACME, "acme", "u1")).await;
    window.say("hello there").await;

    let acme = wait_for_tenant(&hub, "acme").await;
    let incoming = tenant_recv(&acme).await;
    assert_eq!(incoming.text, "hello there");
    // The session is the credential's, not the window's: the browser never
    // named one and could not have.
    assert_eq!(incoming.session.0, "acme/web/u1");

    acme.send(&incoming.session, "and hello to you")
        .await
        .expect("send");
    assert_eq!(window.reply().await.as_deref(), Some("and hello to you"));
}

/// The point of the hub. One customer with a tab open and a desktop app
/// running is one conversation: both hear the answer, and the engine is
/// never told there were two of them.
#[tokio::test]
async fn a_reply_reaches_a_web_window_and_a_desktop_socket_at_once() {
    let hub = Hub::new();
    let http = HttpChannel::bind_on(
        hub.clone(),
        config(),
        verifier(&[("acme", ACME)]),
        Vec::new(),
    )
    .await
    .expect("bind http");
    // The same hub, a second way in: the socket `ns-app serve` has always
    // listened on.
    let tcp = nschannel_tcp::TcpChannel::bind_on(
        hub.clone(),
        "127.0.0.1:0",
        verifier(&[("acme", ACME)]),
        8,
        false,
        Duration::from_secs(2),
    )
    .await
    .expect("bind tcp");

    let mut window = WebClient::open(http.local_addr(), None).await;
    window.hello(&token(ACME, "acme", "u1")).await;
    window.say("from the browser").await;

    let acme = wait_for_tenant(&hub, "acme").await;
    let first = tenant_recv(&acme).await;
    assert_eq!(first.text, "from the browser");

    // The desktop app, on the same credential and therefore the same
    // session.
    let stream = TcpStream::connect(tcp.local_addr()).await.expect("connect");
    let (r, mut w) = stream.into_split();
    let mut desktop = BufReader::new(r);
    let hello = serde_json::json!({ "token": token(ACME, "acme", "u1") });
    w.write_all(format!("{hello}\n").as_bytes())
        .await
        .expect("write");
    w.write_all(b"{\"text\":\"from the desktop\"}\n")
        .await
        .expect("write");

    let second = tenant_recv(&acme).await;
    assert_eq!(second.text, "from the desktop");
    assert_eq!(
        second.session, first.session,
        "one credential, one conversation, whichever way in it arrived by"
    );

    acme.send(&first.session, "to every window")
        .await
        .expect("send");
    assert_eq!(window.reply().await.as_deref(), Some("to every window"));
    let mut line = String::new();
    timeout(T, desktop.read_line(&mut line))
        .await
        .expect("a line in time")
        .expect("read");
    let on_socket: serde_json::Value = serde_json::from_str(&line).expect("a reply object");
    assert_eq!(on_socket["text"], "to every window");
}

/// H3, over this transport too: the credential says which session, and a
/// window that asks for another one does not get it.
#[tokio::test]
async fn a_web_client_cannot_choose_its_session_id() {
    let (http, hub) = bound(Vec::new()).await;
    let mut window = WebClient::open(http.local_addr(), None).await;
    window
        .send(
            &serde_json::json!({
                "token": token(ACME, "acme", "u1"),
                "session": "acme/web/somebody-else",
            })
            .to_string(),
        )
        .await;
    window.say("whose conversation is this").await;

    let acme = wait_for_tenant(&hub, "acme").await;
    assert_eq!(tenant_recv(&acme).await.session.0, "acme/web/u1");
}

#[tokio::test]
async fn a_window_with_a_token_for_another_company_never_reaches_it() {
    let (http, hub) = bound(Vec::new()).await;
    let mut window = WebClient::open(http.local_addr(), None).await;
    // A valid token — for beta — while the window claims to be acme's by
    // asking for an acme session. The issuer is what decides.
    window.hello(&token(BETA, "beta", "u1")).await;
    window.say("hello").await;

    let woken = timeout(T, hub.next_active_tenant())
        .await
        .expect("a wake in time")
        .expect("a name");
    assert_eq!(woken, "beta", "the token's issuer, not the client's wish");
}

#[tokio::test]
async fn a_window_whose_hello_is_refused_is_closed_and_told_nothing() {
    let (http, hub) = bound(Vec::new()).await;
    let mut window = WebClient::open(http.local_addr(), None).await;
    window.hello(&token(b"not-the-key", "acme", "u1")).await;
    assert_eq!(window.close_code().await, Some(4001));
    assert!(
        timeout(QUIET, hub.next_active_tenant()).await.is_err(),
        "nothing reached a company"
    );
}

#[tokio::test]
async fn a_window_that_upgrades_and_says_nothing_is_dropped_at_the_deadline() {
    let (http, _hub) = bound(Vec::new()).await;
    let mut window = WebClient::open(http.local_addr(), None).await;
    // The deadline is two seconds in this config; the close arrives without
    // the client ever sending its hello.
    assert_eq!(window.close_code().await, Some(4001));
}

/// A browser on an origin nobody allowed is refused before the upgrade. The
/// credential is still what keeps a caller out — this is what stops a page
/// elsewhere from spending a visitor's token.
#[tokio::test]
async fn a_browser_on_an_unlisted_origin_is_refused() {
    let (http, _hub) = bound(Vec::new()).await;
    let window = WebClient::open(http.local_addr(), Some("https://evil.test")).await;
    assert!(
        window.status.starts_with("HTTP/1.1 403"),
        "refused before the upgrade, so there is nothing to speak on: {:?}",
        window.status
    );

    let cfg = HttpConfig {
        origins: Origins::These(vec!["https://shop.example".into()]),
        ..config()
    };
    let (http, hub) = bound_with(cfg, Vec::new()).await;
    let mut window = WebClient::open(http.local_addr(), Some("https://shop.example")).await;
    window.hello(&token(ACME, "acme", "u1")).await;
    window.say("hello from the widget").await;
    let acme = wait_for_tenant(&hub, "acme").await;
    assert_eq!(tenant_recv(&acme).await.text, "hello from the widget");
}

// --------------------------------------------------- one message, one reply

/// The `curl` shape: a desktop app or a script that holds nothing open.
#[tokio::test]
async fn a_posted_message_gets_its_reply_in_the_response() {
    let (http, hub) = bound(Vec::new()).await;
    let addr = http.local_addr();
    let auth = format!("authorization: Bearer {}\r\n", token(ACME, "acme", "u1"));

    // The engine side, answering whatever arrives.
    let engine = tokio::spawn({
        let hub = hub.clone();
        async move {
            let acme = wait_for_tenant(&hub, "acme").await;
            let incoming = tenant_recv(&acme).await;
            acme.send(&incoming.session, &format!("you said {}", incoming.text))
                .await
                .expect("send");
            incoming.session
        }
    });

    let (status, body) = request(
        addr,
        &post("/v1/messages", &auth, r#"{"text":"hello over http"}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let answered: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
    assert_eq!(answered["text"], "you said hello over http");
    let session = timeout(T, engine).await.expect("in time").expect("joined");
    assert_eq!(answered["session"], session.0);
}

#[tokio::test]
async fn a_post_without_a_credential_is_refused() {
    let (http, hub) = bound(Vec::new()).await;
    let (status, _) = request(
        http.local_addr(),
        &post("/v1/messages", "", r#"{"text":"hello"}"#),
    )
    .await;
    assert_eq!(status, 401);

    let bad = format!(
        "authorization: Bearer {}\r\n",
        token(b"wrong", "acme", "u1")
    );
    let (status, _) = request(
        http.local_addr(),
        &post("/v1/messages", &bad, r#"{"text":"hello"}"#),
    )
    .await;
    assert_eq!(status, 401);
    assert!(
        timeout(QUIET, hub.next_active_tenant()).await.is_err(),
        "nothing reached a company"
    );
}

/// A turn that takes longer than the caller will wait is not an error and
/// not a lost message: it is still running, and the answer is in the log and
/// on any window holding the session.
#[tokio::test]
async fn a_turn_that_outlasts_the_wait_is_a_504_and_keeps_running() {
    let cfg = HttpConfig {
        reply_timeout: Duration::from_millis(300),
        ..config()
    };
    let (http, hub) = bound_with(cfg, Vec::new()).await;
    let auth = format!("authorization: Bearer {}\r\n", token(ACME, "acme", "u1"));
    let posting = tokio::spawn({
        let addr = http.local_addr();
        async move { request(addr, &post("/v1/messages", &auth, r#"{"text":"slow"}"#)).await }
    });

    let acme = wait_for_tenant(&hub, "acme").await;
    let incoming = tenant_recv(&acme).await;
    let (status, body) = timeout(T, posting).await.expect("in time").expect("joined");
    assert_eq!(status, 504, "{body}");
    assert!(body.contains(&incoming.session.0), "{body}");
    // And the turn's answer still has somewhere to go.
    acme.send(&incoming.session, "late but here")
        .await
        .expect("send");
}

#[tokio::test]
async fn health_is_answered_without_a_credential_and_says_nothing_else() {
    let (http, _hub) = bound(Vec::new()).await;
    let (status, body) = request(
        http.local_addr(),
        "GET /healthz HTTP/1.1\r\nHost: x\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn an_unknown_route_is_a_404() {
    let (http, _hub) = bound(Vec::new()).await;
    let (status, _) = request(
        http.local_addr(),
        "GET /admin HTTP/1.1\r\nHost: x\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 404);
}

// ------------------------------------------------------------- a platform

const WA_SECRET: &str = "acme-app-secret";

fn whatsapp() -> (Arc<PlatformEndpoint>, Arc<MockReplyTransport>) {
    let transport = MockReplyTransport::new(vec![]);
    let adapter = WhatsApp::new(
        vec![Account {
            phone_number_id: "111".into(),
            tenant: "acme".into(),
            access_token: "acme-graph-token".into(),
            app_secret: WA_SECRET.into(),
            session_salt: "acme-salt".into(),
        }],
        "verify-me".into(),
    );
    (
        PlatformEndpoint::new(Arc::new(adapter), transport.clone(), SeenIds::in_memory()),
        transport,
    )
}

fn delivery(id: &str, at: &str, text: &str) -> String {
    serde_json::json!({
        "entry": [{ "changes": [{ "value": {
            "metadata": { "phone_number_id": "111" },
            "messages": [{
                "from": "15551234567", "id": id, "timestamp": at,
                "type": "text", "text": { "body": text },
            }],
        }}]}],
    })
    .to_string()
}

fn signature(secret: &str, body: &str) -> String {
    let digest = hmac_sha256(secret.as_bytes(), body.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("x-hub-signature-256: sha256={hex}\r\n")
}

/// The whole webhook path: a signed delivery becomes a turn, and the answer
/// goes out as a call to the platform's API rather than down a socket.
#[tokio::test]
async fn a_signed_delivery_runs_a_turn_and_its_reply_goes_back_to_the_platform() {
    let (endpoint, transport) = whatsapp();
    let (http, hub) = bound(vec![endpoint]).await;
    let body = delivery("wamid.1", "1700000000", "is my order ready");
    let (status, answered) = request(
        http.local_addr(),
        &post("/hooks/whatsapp", &signature(WA_SECRET, &body), &body),
    )
    .await;
    // Answered at once: the turn takes seconds and a webhook must not.
    assert_eq!(status, 200, "{answered}");

    let acme = wait_for_tenant(&hub, "acme").await;
    let incoming = tenant_recv(&acme).await;
    assert_eq!(incoming.text, "is my order ready");
    assert!(
        incoming.session.0.starts_with("acme/whatsapp/"),
        "{}",
        incoming.session.0
    );
    assert!(
        !incoming.session.0.contains("15551234"),
        "a phone number must not become a session id: {}",
        incoming.session.0
    );

    acme.send(&incoming.session, "yes, it ships today")
        .await
        .expect("send");
    // The reply leaves as a Graph call. Polled rather than awaited: it is
    // sent by the endpoint's own task.
    let sent = timeout(T, async {
        loop {
            let sent = transport.sent();
            if !sent.is_empty() {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a reply out in time");
    assert_eq!(sent.len(), 1);
    assert!(sent[0].0.contains("/111/messages"), "{}", sent[0].0);
    assert_eq!(sent[0].1["text"]["body"], "yes, it ships today");
    assert_eq!(sent[0].1["to"], "15551234567");
}

/// H4. The platform retries anything it did not get a 200 for; the turn must
/// run once, or the customer is answered twice and the company billed twice.
#[tokio::test]
async fn a_repeated_message_id_runs_one_turn() {
    let (endpoint, _transport) = whatsapp();
    let (http, hub) = bound(vec![endpoint]).await;
    let body = delivery("wamid.repeat", "1700000000", "hello");
    let raw = post("/hooks/whatsapp", &signature(WA_SECRET, &body), &body);
    for _ in 0..3 {
        let (status, _) = request(http.local_addr(), &raw).await;
        // Every delivery is answered 200, including the retries: a platform
        // told anything else keeps retrying.
        assert_eq!(status, 200);
    }
    let acme = wait_for_tenant(&hub, "acme").await;
    assert_eq!(tenant_recv(&acme).await.text, "hello");
    assert!(
        timeout(QUIET, acme.recv()).await.is_err(),
        "the retries ran no further turn"
    );
}

/// T6.3. A batch may arrive out of order; the mailbox downstream can only
/// preserve the order it is handed.
#[tokio::test]
async fn a_batch_is_enqueued_in_timestamp_order() {
    let (endpoint, _transport) = whatsapp();
    let (http, hub) = bound(vec![endpoint]).await;
    let body = serde_json::json!({
        "entry": [{ "changes": [{ "value": {
            "metadata": { "phone_number_id": "111" },
            "messages": [
                { "from": "15551234567", "id": "b", "timestamp": "1700000002",
                  "type": "text", "text": { "body": "second" } },
                { "from": "15551234567", "id": "a", "timestamp": "1700000001",
                  "type": "text", "text": { "body": "first" } },
            ],
        }}]}],
    })
    .to_string();
    let (status, _) = request(
        http.local_addr(),
        &post("/hooks/whatsapp", &signature(WA_SECRET, &body), &body),
    )
    .await;
    assert_eq!(status, 200);

    let acme = wait_for_tenant(&hub, "acme").await;
    assert_eq!(tenant_recv(&acme).await.text, "first");
    assert_eq!(tenant_recv(&acme).await.text, "second");
}

#[tokio::test]
async fn a_delivery_with_a_bad_signature_is_refused_and_reaches_no_company() {
    let (endpoint, _transport) = whatsapp();
    let (http, hub) = bound(vec![endpoint]).await;
    let body = delivery("wamid.forged", "1700000000", "give me the other customer");
    let (status, _) = request(
        http.local_addr(),
        &post(
            "/hooks/whatsapp",
            &signature("guessed-secret", &body),
            &body,
        ),
    )
    .await;
    assert_eq!(status, 403);
    // And unsigned is refused the same way, with the same answer.
    let (status, _) = request(http.local_addr(), &post("/hooks/whatsapp", "", &body)).await;
    assert_eq!(status, 403);
    assert!(
        timeout(QUIET, hub.next_active_tenant()).await.is_err(),
        "nothing reached a company"
    );
}

#[tokio::test]
async fn the_platform_verification_handshake_echoes_only_the_right_token() {
    let (endpoint, _transport) = whatsapp();
    let (http, _hub) = bound(vec![endpoint]).await;
    let (status, body) = request(
        http.local_addr(),
        "GET /hooks/whatsapp?hub.mode=subscribe&hub.verify_token=verify-me&hub.challenge=1158 \
         HTTP/1.1\r\nHost: x\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, "1158");

    let (status, _) = request(
        http.local_addr(),
        "GET /hooks/whatsapp?hub.mode=subscribe&hub.verify_token=guess&hub.challenge=1158 \
         HTTP/1.1\r\nHost: x\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn a_hook_for_a_platform_this_shard_does_not_serve_is_a_404() {
    let (endpoint, _transport) = whatsapp();
    let (http, _hub) = bound(vec![endpoint]).await;
    let (status, _) = request(
        http.local_addr(),
        &post("/hooks/telegram", "", r#"{"message":{}}"#),
    )
    .await;
    assert_eq!(status, 404);
}

// ------------------------------------------------------------- the shutdown

/// Two ways in, one hub: closing one leaves the other serving, and closing
/// the last one is the shutdown. Before the hub, "the listener was dropped"
/// was the whole of a shard's shutdown, and it cannot be any more.
#[tokio::test]
async fn the_shard_stays_up_while_any_way_in_is_open() {
    let hub = Hub::new();
    let http = HttpChannel::bind_on(
        hub.clone(),
        config(),
        verifier(&[("acme", ACME)]),
        Vec::new(),
    )
    .await
    .expect("bind http");
    let tcp = nschannel_tcp::TcpChannel::bind_on(
        hub.clone(),
        "127.0.0.1:0",
        verifier(&[("acme", ACME)]),
        8,
        false,
        Duration::from_secs(2),
    )
    .await
    .expect("bind tcp");

    drop(tcp);
    assert!(!hub.is_closed(), "the HTTP way in is still open");
    let mut window = WebClient::open(http.local_addr(), None).await;
    window.hello(&token(ACME, "acme", "u1")).await;
    window.say("still serving").await;
    let acme = wait_for_tenant(&hub, "acme").await;
    assert_eq!(tenant_recv(&acme).await.text, "still serving");

    drop(acme);
    drop(http);
    assert!(hub.is_closed(), "the last way in closed is the shutdown");
}

//! End-to-end over an in-memory duplex: real client, real agent, real
//! protocol, no socket and no desktop. Doubles as the conformance suite for
//! an agent written in another language — every assertion here is a rule
//! `docs/pointer-protocol.md` states.

use nspointer::agent::{Agent, AgentConfig, Audit, Limits};
use nspointer::client::RemotePointer;
use nspointer::platform::{NullPlatform, Platform};
use nspointer::wire::{ErrorKind, InputError, Key, Step};
use nspointer::{Button, Loc, Point, Pointer, Rect, Screen, ScreenId, Screens, Session};
use std::sync::Arc;
use tokio::io::BufReader;

const TOKEN: &str = "correct-horse-battery-staple";

fn layout() -> Screens {
    Screens {
        screens: vec![Screen {
            id: ScreenId::from("S1"),
            bounds: Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
            scale: 1.0,
            primary: true,
            label: "test".into(),
        }],
        state: 3,
    }
}

#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<serde_json::Value>>);

/// The orphan rule forbids `impl Audit for Arc<Recorder>`, and a handle is
/// what an agent would hold anyway.
struct AuditHandle(Arc<Recorder>);
impl Audit for AuditHandle {
    fn record(&self, entry: &serde_json::Value) {
        self.0 .0.lock().unwrap().push(entry.clone());
    }
}
impl Recorder {
    fn events(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|e| e["event"].as_str().unwrap_or("?").to_string())
            .collect()
    }
}

/// Spawns an agent on one end of a duplex and hands back a connected client.
/// `clock` lets a test drive the rate limiter and the override without
/// sleeping.
async fn connect<P: Platform + 'static>(
    platform: Arc<P>,
    limits: Limits,
    audit: Option<Arc<Recorder>>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    token: &str,
) -> Result<
    RemotePointer<
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
    InputError,
> {
    let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
    let (ar, aw) = tokio::io::split(agent_side);
    let mut agent = Agent::new(
        platform,
        AgentConfig {
            token: TOKEN.into(),
            limits,
        },
    )
    .with_clock(Box::new(move || clock()));
    if let Some(a) = audit {
        agent = agent.with_audit(Box::new(AuditHandle(a)));
    }
    tokio::spawn(async move {
        let _ = agent.serve(ar, aw).await;
    });
    let (cr, cw) = tokio::io::split(client_side);
    RemotePointer::connect(BufReader::new(cr), cw, token).await
}

fn fixed_clock(ms: u64) -> Arc<dyn Fn() -> u64 + Send + Sync> {
    Arc::new(move || ms)
}

#[tokio::test]
async fn a_gesture_survives_the_round_trip_intact() {
    let p = Arc::new(NullPlatform::new(layout()));
    let c = connect(p.clone(), Limits::default(), None, fixed_clock(0), TOKEN)
        .await
        .unwrap();
    let s = Session::open(c).await.unwrap();
    s.click_at(&Loc::normalized("S1", 0.5, 0.5), Button::Left, 1)
        .await
        .unwrap();

    let log = p.log();
    assert!(log.iter().any(|l| l == "button Left true"));
    assert!(log.iter().any(|l| l == "button Left false"));
    // Landed exactly where the caller aimed: (1919·0.5, 1079·0.5) rounded.
    assert_eq!(
        log.iter().rev().find(|l| l.starts_with("move")).unwrap(),
        "move 960,540"
    );
}

#[tokio::test]
async fn nothing_works_before_a_valid_hello() {
    let p = Arc::new(NullPlatform::new(layout()));
    // Wrong token: the connection is refused at `connect`, so no later call
    // has to remember to check.
    let err = match connect(p.clone(), Limits::default(), None, fixed_clock(0), "guess").await {
        Err(e) => e,
        Ok(_) => panic!("a wrong token must not produce a connection"),
    };
    assert!(
        matches!(
            err,
            InputError::Agent {
                kind: ErrorKind::Unauthorized,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        p.log().is_empty(),
        "an unauthenticated caller moved nothing"
    );
}

#[tokio::test]
async fn an_oversized_batch_is_refused_whole() {
    let p = Arc::new(NullPlatform::new(layout()));
    let limits = Limits {
        max_steps: 10,
        ..Limits::default()
    };
    let c = connect(p.clone(), limits, None, fixed_clock(0), TOKEN)
        .await
        .unwrap();
    let steps: Vec<Step> = (0..11).map(|i| Step::Move { x: i, y: 0 }).collect();
    let err = c.perform(&steps).await.unwrap_err();
    assert!(
        matches!(
            err,
            InputError::Agent {
                kind: ErrorKind::Protocol,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(p.log().is_empty(), "refused whole, not partway");
}

/// A limit the client applies to itself is not a limit, so this is enforced
/// where a caller cannot reach it — and proven with a clock rather than a
/// sleep.
#[tokio::test]
async fn the_rate_limit_is_enforced_and_refills() {
    let p = Arc::new(NullPlatform::new(layout()));
    let now = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let n = now.clone();
    let limits = Limits {
        performs_per_sec: 10.0,
        burst: 2.0,
        ..Limits::default()
    };
    let c = connect(
        p.clone(),
        limits,
        None,
        Arc::new(move || n.load(std::sync::atomic::Ordering::SeqCst)),
        TOKEN,
    )
    .await
    .unwrap();

    let one = [Step::Scroll { dx: 0, dy: 1 }];
    assert!(c.perform(&one).await.is_ok());
    assert!(c.perform(&one).await.is_ok());
    let err = c.perform(&one).await.unwrap_err();
    assert!(
        matches!(
            err,
            InputError::Agent {
                kind: ErrorKind::Internal,
                ..
            }
        ),
        "{err:?}"
    );

    // 200ms at 10/s refills two tokens.
    now.store(200, std::sync::atomic::Ordering::SeqCst);
    assert!(c.perform(&one).await.is_ok(), "the bucket refills");
}

/// The person at the keyboard outranks the socket.
#[tokio::test]
async fn local_activity_suspends_remote_input_until_it_lapses() {
    let p = Arc::new(NullPlatform::new(layout()));
    let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let n = now.clone();
    let limits = Limits {
        suspend_ms: 3_000,
        ..Limits::default()
    };
    let c = connect(
        p.clone(),
        limits,
        None,
        Arc::new(move || n.load(std::sync::atomic::Ordering::SeqCst)),
        TOKEN,
    )
    .await
    .unwrap();

    let one = [Step::Scroll { dx: 0, dy: 1 }];
    assert!(c.perform(&one).await.is_ok());

    // A human touches the machine.
    p.local.store(true, std::sync::atomic::Ordering::SeqCst);
    let err = c.perform(&one).await.unwrap_err();
    assert!(
        matches!(
            err,
            InputError::Agent {
                kind: ErrorKind::Suspended,
                ..
            }
        ),
        "{err:?}"
    );
    // Still suspended a second later, and distinct from a rate limit so the
    // caller can tell "wait" from "back off".
    now.store(2_000, std::sync::atomic::Ordering::SeqCst);
    let err = c.perform(&one).await.unwrap_err();
    assert!(matches!(
        err,
        InputError::Agent {
            kind: ErrorKind::Suspended,
            ..
        }
    ));
    // ...and released once it lapses.
    now.store(4_500, std::sync::atomic::Ordering::SeqCst);
    assert!(c.perform(&one).await.is_ok());
}

/// A stuck Ctrl is not a failed operation, it is an unusable machine. The
/// client cannot fix this — the failure is precisely when it has no second
/// half of the batch to send.
#[tokio::test]
async fn a_failed_batch_releases_what_it_had_pressed() {
    struct FailsOnText(Screens, std::sync::Mutex<Vec<String>>);
    impl Platform for FailsOnText {
        fn screens(&self) -> Result<Screens, InputError> {
            Ok(self.0.clone())
        }
        fn position(&self) -> Result<Point, InputError> {
            Ok(Point::new(0, 0))
        }
        fn move_to(&self, _: Point) -> Result<(), InputError> {
            Ok(())
        }
        fn button(&self, b: Button, d: bool) -> Result<(), InputError> {
            self.1.lock().unwrap().push(format!("button {b:?} {d}"));
            Ok(())
        }
        fn scroll(&self, _: i32, _: i32) -> Result<(), InputError> {
            Ok(())
        }
        fn key(&self, k: &Key, d: bool) -> Result<(), InputError> {
            self.1.lock().unwrap().push(format!("key {k:?} {d}"));
            Ok(())
        }
        fn text(&self, _: &str) -> Result<(), InputError> {
            Err(InputError::Agent {
                kind: ErrorKind::Blocked,
                detail: "elevated".into(),
            })
        }
        fn local_activity(&self) -> bool {
            false
        }
    }
    let p = Arc::new(FailsOnText(layout(), Default::default()));
    let c = connect(p.clone(), Limits::default(), None, fixed_clock(0), TOKEN)
        .await
        .unwrap();

    let err = c
        .perform(&[
            Step::Key {
                key: Key::Ctrl,
                down: true,
            },
            Step::Button {
                button: Button::Left,
                down: true,
            },
            Step::Text { text: "x".into() }, // refused here
            Step::Key {
                key: Key::Ctrl,
                down: false,
            },
        ])
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            InputError::Agent {
                kind: ErrorKind::Blocked,
                ..
            }
        ),
        "{err:?}"
    );

    let log = p.1.lock().unwrap().clone();
    assert_eq!(
        log,
        vec![
            "key Ctrl true",
            "button Left true",
            // the agent cleaned up after the batch it could not finish
            "key Ctrl false",
            "button Left false",
        ]
    );
}

#[tokio::test]
async fn held_keys_are_released_when_the_connection_drops() {
    let p = Arc::new(NullPlatform::new(layout()));
    let audit = Arc::new(Recorder::default());
    {
        let c = connect(
            p.clone(),
            Limits::default(),
            Some(audit.clone()),
            fixed_clock(0),
            TOKEN,
        )
        .await
        .unwrap();
        c.perform(&[Step::Key {
            key: Key::Ctrl,
            down: true,
        }])
        .await
        .unwrap();
        assert_eq!(p.log(), vec!["key Ctrl true"]);
        // client dropped here: the socket closes with Ctrl still down
    }
    // Give the agent task its turn to notice the close.
    for _ in 0..50 {
        tokio::task::yield_now().await;
        if p.log().len() > 1 {
            break;
        }
    }
    assert_eq!(p.log(), vec!["key Ctrl true", "key Ctrl false"]);
    assert!(audit.events().contains(&"release_held".to_string()));
}

#[tokio::test]
async fn the_audit_records_what_was_injected_not_what_was_asked() {
    let p = Arc::new(NullPlatform::new(layout()));
    let audit = Arc::new(Recorder::default());
    let c = connect(
        p.clone(),
        Limits::default(),
        Some(audit.clone()),
        fixed_clock(7),
        TOKEN,
    )
    .await
    .unwrap();
    c.perform(&[Step::Scroll { dx: 0, dy: 1 }]).await.unwrap();
    let entries = audit.0.lock().unwrap().clone();
    let performed = entries.iter().find(|e| e["event"] == "performed").unwrap();
    assert_eq!(performed["steps"], 1);
    assert_eq!(performed["at"], 7);
}

#[tokio::test]
async fn a_bad_protocol_version_is_refused_rather_than_guessed() {
    let p = Arc::new(NullPlatform::new(layout()));
    let (client_side, agent_side) = tokio::io::duplex(4096);
    let (ar, aw) = tokio::io::split(agent_side);
    let agent = Agent::new(
        NullPlatform::new(layout()),
        AgentConfig {
            token: TOKEN.into(),
            limits: Limits::default(),
        },
    );
    tokio::spawn(async move {
        let _ = agent.serve(ar, aw).await;
    });
    let (cr, cw) = tokio::io::split(client_side);
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut w = cw;
    w.write_all(
        format!("{{\"id\":1,\"op\":\"hello\",\"token\":\"{TOKEN}\",\"protocol\":99}}\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut line = String::new();
    BufReader::new(cr).read_line(&mut line).await.unwrap();
    assert!(line.contains("\"kind\":\"protocol\""), "{line}");
    let _ = p;
}

#[tokio::test]
async fn an_unparseable_line_is_answered_not_fatal() {
    let (client_side, agent_side) = tokio::io::duplex(4096);
    let (ar, aw) = tokio::io::split(agent_side);
    let agent = Agent::new(
        NullPlatform::new(layout()),
        AgentConfig {
            token: TOKEN.into(),
            limits: Limits::default(),
        },
    );
    tokio::spawn(async move {
        let _ = agent.serve(ar, aw).await;
    });
    let (cr, cw) = tokio::io::split(client_side);
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut w = cw;
    let mut r = BufReader::new(cr);
    w.write_all(b"{not json\n").await.unwrap();
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    assert!(line.contains("\"kind\":\"protocol\""), "{line}");
    // and the connection is still usable
    w.write_all(
        format!(
            "{{\"id\":2,\"op\":\"hello\",\"token\":\"{TOKEN}\",\"protocol\":{}}}\n",
            nspointer::PROTOCOL
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    line.clear();
    r.read_line(&mut line).await.unwrap();
    assert!(line.contains("\"kind\":\"ready\""), "{line}");
}

/// Clipboard is optional: an agent that has not implemented it answers
/// `Unsupported` rather than failing to build, and a caller can tell the
/// difference between "not available" and "went wrong".
#[tokio::test]
async fn an_agent_without_a_clipboard_says_so() {
    struct NoClipboard(Screens);
    impl Platform for NoClipboard {
        fn screens(&self) -> Result<Screens, InputError> {
            Ok(self.0.clone())
        }
        fn position(&self) -> Result<Point, InputError> {
            Ok(Point::new(0, 0))
        }
        fn move_to(&self, _: Point) -> Result<(), InputError> {
            Ok(())
        }
        fn button(&self, _: Button, _: bool) -> Result<(), InputError> {
            Ok(())
        }
        fn scroll(&self, _: i32, _: i32) -> Result<(), InputError> {
            Ok(())
        }
        fn key(&self, _: &Key, _: bool) -> Result<(), InputError> {
            Ok(())
        }
        fn text(&self, _: &str) -> Result<(), InputError> {
            Ok(())
        }
        fn local_activity(&self) -> bool {
            false
        }
        // clipboard_read / clipboard_write deliberately not implemented
    }
    let c = connect(
        Arc::new(NoClipboard(layout())),
        Limits::default(),
        None,
        fixed_clock(0),
        TOKEN,
    )
    .await
    .unwrap();
    let err = c.clipboard_read().await.unwrap_err();
    assert!(
        matches!(
            err,
            InputError::Agent {
                kind: ErrorKind::Unsupported,
                ..
            }
        ),
        "{err:?}"
    );
}

/// The round trip, and the audit rule that goes with it.
#[tokio::test]
async fn the_clipboard_round_trips_without_its_contents_reaching_the_log() {
    let p = Arc::new(NullPlatform::new(layout()));
    let audit = Arc::new(Recorder::default());
    let c = connect(
        p.clone(),
        Limits::default(),
        Some(audit.clone()),
        fixed_clock(0),
        TOKEN,
    )
    .await
    .unwrap();

    c.clipboard_write("hunter2 is not a good password")
        .await
        .unwrap();
    assert_eq!(
        c.clipboard_read().await.unwrap(),
        "hunter2 is not a good password"
    );

    // A machine's clipboard holds secrets often enough that logging it would
    // turn the audit trail into the leak. Length only.
    let entries = audit.0.lock().unwrap().clone();
    let text = serde_json::to_string(&entries).unwrap();
    assert!(!text.contains("hunter2"), "{text}");
    assert!(text.contains("\"chars\":30"), "{text}");
    assert!(audit.events().contains(&"clipboard_write".to_string()));
}

// ---- the accept loop (plan 2026-09-04-pointer-integration, phase A1) ----

use nspointer::agent::{bind, serve_listener, Listen};

fn cfg(limits: Limits) -> AgentConfig {
    AgentConfig {
        token: TOKEN.into(),
        limits,
    }
}

/// Starts a real listener on an ephemeral port and returns its address.
async fn listening(
    platform: Arc<NullPlatform>,
    limits: Limits,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> String {
    let c = cfg(limits);
    let agent = Agent::new(platform, c).with_clock(Box::new(move || clock()));
    let listener = bind(
        &AgentConfig {
            token: TOKEN.into(),
            limits: Limits::default(),
        },
        &Listen::loopback(0),
    )
    .await
    .unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = serve_listener(Arc::new(agent), listener).await;
    });
    addr
}

async fn dial(
    addr: &str,
    token: &str,
) -> Result<
    RemotePointer<
        BufReader<tokio::io::ReadHalf<tokio::net::TcpStream>>,
        tokio::io::WriteHalf<tokio::net::TcpStream>,
    >,
    InputError,
> {
    let s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (r, w) = tokio::io::split(s);
    RemotePointer::connect(BufReader::new(r), w, token).await
}

/// The whole chain over a real socket, which is what `ns-pointerd` will do.
#[tokio::test]
async fn a_real_socket_carries_the_protocol() {
    let p = Arc::new(NullPlatform::new(layout()));
    let addr = listening(p.clone(), Limits::default(), fixed_clock(0)).await;

    let c = dial(&addr, TOKEN).await.unwrap();
    let s = Session::open(c).await.unwrap();
    s.click_at(&Loc::normalized("S1", 0.5, 0.5), Button::Left, 1)
        .await
        .unwrap();
    assert!(p.log().iter().any(|l| l == "button Left true"));
    assert_eq!(
        p.log()
            .iter()
            .rev()
            .find(|l| l.starts_with("move"))
            .unwrap(),
        "move 960,540"
    );

    // A wrong token gets nowhere, over a socket as over a pipe.
    assert!(dial(&addr, "guess").await.is_err());
}

/// The bug the accept loop surfaced. A rate limit held per socket is defeated
/// by opening a second socket — so it is machine-wide, and this proves it.
#[tokio::test]
async fn the_rate_limit_is_machine_wide_not_per_connection() {
    let p = Arc::new(NullPlatform::new(layout()));
    let now = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let n = now.clone();
    let addr = listening(
        p.clone(),
        Limits {
            performs_per_sec: 1.0,
            burst: 2.0,
            ..Limits::default()
        },
        Arc::new(move || n.load(std::sync::atomic::Ordering::SeqCst)),
    )
    .await;

    let one = [Step::Scroll { dx: 0, dy: 1 }];
    let a = dial(&addr, TOKEN).await.unwrap();
    assert!(a.perform(&one).await.is_ok());
    assert!(a.perform(&one).await.is_ok());
    assert!(a.perform(&one).await.is_err(), "budget of 2 is spent");

    // A second connection does not get a fresh budget.
    let b = dial(&addr, TOKEN).await.unwrap();
    let err = b.perform(&one).await.unwrap_err();
    assert!(
        matches!(
            err,
            InputError::Agent {
                kind: ErrorKind::Internal,
                ..
            }
        ),
        "reconnecting must not multiply the rate: {err:?}"
    );
}

/// The override protects a desktop, not a socket: one connection tripping it
/// must stop the others too.
#[tokio::test]
async fn the_local_override_is_machine_wide() {
    let p = Arc::new(NullPlatform::new(layout()));
    let addr = listening(p.clone(), Limits::default(), fixed_clock(1_000)).await;
    let a = dial(&addr, TOKEN).await.unwrap();
    let b = dial(&addr, TOKEN).await.unwrap();
    let one = [Step::Scroll { dx: 0, dy: 1 }];
    assert!(a.perform(&one).await.is_ok());

    p.local.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(a.perform(&one).await.is_err(), "the connection that saw it");
    let err = b.perform(&one).await.unwrap_err();
    assert!(
        matches!(
            err,
            InputError::Agent {
                kind: ErrorKind::Suspended,
                ..
            }
        ),
        "and every other one: {err:?}"
    );
}

/// Both refusals are hard errors, because both failure modes are silent and
/// permanent: no token accepts anyone who finds the port, and a stray bind
/// address is an input-injection service on the network.
#[tokio::test]
async fn binding_refuses_the_two_configurations_that_would_be_mistakes() {
    let no_token = AgentConfig {
        token: String::new(),
        limits: Limits::default(),
    };
    let e = bind(&no_token, &Listen::loopback(0)).await.unwrap_err();
    assert!(e.to_string().contains("empty token"), "{e}");

    let exposed = Listen {
        addr: "0.0.0.0:0".into(),
        allow_remote: false,
    };
    let e = bind(&cfg(Limits::default()), &exposed).await.unwrap_err();
    assert!(e.to_string().contains("allow_remote"), "{e}");

    // Meaning it is allowed; the point is that it cannot happen by typo.
    assert!(bind(
        &cfg(Limits::default()),
        &Listen {
            addr: "0.0.0.0:0".into(),
            allow_remote: true
        }
    )
    .await
    .is_ok());
}

#[tokio::test]
async fn connections_past_the_cap_are_told_why_before_the_hangup() {
    let p = Arc::new(NullPlatform::new(layout()));
    let addr = listening(
        p,
        Limits {
            max_connections: 1,
            ..Limits::default()
        },
        fixed_clock(0),
    )
    .await;
    let _first = dial(&addr, TOKEN).await.unwrap();

    // The second is refused with a reason rather than a silent close: a
    // caller cannot otherwise tell a full agent from a dead one.
    let s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let (r, _w) = tokio::io::split(s);
    use tokio::io::AsyncBufReadExt as _;
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    assert!(line.contains("too many connections"), "{line}");
}

/// A dead override is the one failure a caller cannot infer for itself — the
/// agent simply never says `suspended`. So it is reported, and the default is
/// the pessimistic one: an agent that has not said it installed a hook is
/// assumed not to have one.
#[tokio::test]
async fn an_agent_with_no_local_override_says_so_rather_than_looking_healthy() {
    struct NoHook(Screens);
    impl Platform for NoHook {
        fn screens(&self) -> Result<Screens, InputError> {
            Ok(self.0.clone())
        }
        fn position(&self) -> Result<Point, InputError> {
            Ok(Point::new(0, 0))
        }
        fn move_to(&self, _: Point) -> Result<(), InputError> {
            Ok(())
        }
        fn button(&self, _: Button, _: bool) -> Result<(), InputError> {
            Ok(())
        }
        fn scroll(&self, _: i32, _: i32) -> Result<(), InputError> {
            Ok(())
        }
        fn key(&self, _: &Key, _: bool) -> Result<(), InputError> {
            Ok(())
        }
        fn text(&self, _: &str) -> Result<(), InputError> {
            Ok(())
        }
        fn local_activity(&self) -> bool {
            false
        }
        // local_hook_ok deliberately not implemented: the default is `false`.
    }
    let audit = Arc::new(Recorder::default());
    let (client, srv) = tokio::io::duplex(64 * 1024);
    let (ar, aw) = tokio::io::split(srv);
    let agent = Agent::new(NoHook(layout()), cfg(Limits::default()))
        .with_audit(Box::new(AuditHandle(audit.clone())));
    tokio::spawn(async move {
        let _ = agent.serve(ar, aw).await;
    });
    let (cr, cw) = tokio::io::split(client);
    let p = RemotePointer::connect(BufReader::new(cr), cw, TOKEN)
        .await
        .unwrap();

    assert!(
        !p.local_override(),
        "the brake is dead and the client knows"
    );
    assert!(audit.events().contains(&"no_local_override".to_string()));

    // And an agent that installed one reports the opposite, over the same path.
    let plat = Arc::new(NullPlatform::new(layout()));
    let c = connect(plat, Limits::default(), None, fixed_clock(0), TOKEN)
        .await
        .unwrap();
    assert!(c.local_override());
}

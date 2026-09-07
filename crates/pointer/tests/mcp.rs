//! The MCP surface, driven the way a client drives it: JSON-RPC lines in,
//! JSON-RPC lines out, over an in-memory pipe.

use nspointer::mcp::{parse_key, Confirm, McpServer, MCP_PROTOCOL};
use nspointer::mock::MockPointer;
use nspointer::wire::{ErrorKind, InputError, Key, Step};
use nspointer::{Point, Rect, Screen, ScreenId, Screens, Session};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn layout() -> Screens {
    Screens {
        screens: vec![
            Screen {
                id: ScreenId::from("S1"),
                bounds: Rect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                },
                scale: 1.0,
                primary: true,
                label: "main".into(),
            },
            Screen {
                id: ScreenId::from("S2"),
                bounds: Rect {
                    x: -1280,
                    y: 0,
                    w: 1280,
                    h: 1024,
                },
                scale: 1.25,
                primary: false,
                label: "left".into(),
            },
        ],
        state: 11,
    }
}

/// Drives a server over a duplex and returns one response per request sent.
async fn talk(reqs: Vec<Value>) -> (Vec<Value>, Arc<MockPointer>) {
    let mock = Arc::new(MockPointer::new(layout(), Point::new(0, 0)));
    struct Shared(Arc<MockPointer>);
    #[async_trait::async_trait]
    impl nspointer::Pointer for Shared {
        async fn screens(&self) -> Result<Screens, InputError> {
            self.0.screens().await
        }
        async fn position(&self) -> Result<Point, InputError> {
            self.0.position().await
        }
        async fn perform(&self, steps: &[Step]) -> Result<u64, InputError> {
            self.0.perform(steps).await
        }
        // Defaulted methods need forwarding: a wrapper that omits them
        // silently reports the capability unsupported.
        async fn clipboard_read(&self) -> Result<String, InputError> {
            self.0.clipboard_read().await
        }
        async fn clipboard_write(&self, t: &str) -> Result<(), InputError> {
            self.0.clipboard_write(t).await
        }
    }
    let session = Session::open(Shared(mock.clone())).await.unwrap();
    // Existing behaviour tests run ungated; the gate has its own below.
    let server = McpServer::new(session).with_confirm(Confirm::Off);

    let (client, srv) = tokio::io::duplex(256 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    tokio::spawn(async move {
        let _ = server.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();

    let mut out = Vec::new();
    for r in reqs {
        let has_id = r.get("id").is_some();
        let mut b = serde_json::to_vec(&r).unwrap();
        b.push(b'\n');
        cw.write_all(&b).await.unwrap();
        cw.flush().await.unwrap();
        if has_id {
            let line = lines.next_line().await.unwrap().unwrap();
            out.push(serde_json::from_str(&line).unwrap());
        }
    }
    (out, mock)
}

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
           "params":{"name":name,"arguments":args}})
}

#[tokio::test]
async fn initialize_and_list_report_a_usable_surface() {
    let (r, _) = talk(vec![
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ])
    .await;
    assert_eq!(r.len(), 2, "a notification must not be answered");
    assert_eq!(r[0]["result"]["protocolVersion"], MCP_PROTOCOL);
    assert_eq!(r[0]["result"]["serverInfo"]["name"], "ns-pointer");

    let names: Vec<&str> = r[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "screens_list",
            "pointer_position",
            "pointer_move",
            "pointer_click",
            "pointer_drag",
            "pointer_scroll",
            "type_text",
            "ui_read",
            "ui_find",
            "clipboard_read",
            "clipboard_write",
            "key_press",
        ]
    );
    // Every tool carries a schema a client can render.
    for t in r[1]["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        assert!(t["description"].as_str().unwrap().len() > 20);
    }
}

/// The convention every comparable server uses, and what a model reaches for.
#[tokio::test]
async fn coordinates_are_absolute_pixels_by_default() {
    let (r, mock) = talk(vec![call(1, "pointer_move", json!({"x": 800, "y": 400}))]).await;
    assert_eq!(r[0]["result"]["isError"], false);
    assert_eq!(*mock.track().last().unwrap(), Point::new(800, 400));
}

/// ...and naming a screen switches to fractions of it, which is the precise
/// form once a caller has read the layout.
#[tokio::test]
async fn naming_a_screen_switches_to_fractions_of_it() {
    let (r, mock) = talk(vec![call(
        1,
        "pointer_move",
        json!({"x": 0.5, "y": 0.5, "screen": "S2"}),
    )])
    .await;
    assert_eq!(r[0]["result"]["isError"], false);
    // S2 is at x = -1280, 1280x1024 -> centre is (-1280 + 640, 512).
    assert_eq!(*mock.track().last().unwrap(), Point::new(-640, 512));
}

#[tokio::test]
async fn screens_list_returns_the_geometry_a_screenshot_needs() {
    let (r, _) = talk(vec![call(1, "screens_list", json!({}))]).await;
    let s = &r[0]["result"]["structuredContent"];
    assert_eq!(s["state"], 11);
    let first = &s["screens"][0];
    assert_eq!(first["id"], "S1");
    assert_eq!(first["bounds"]["w"], 1920);
    assert_eq!(
        s["screens"][1]["bounds"]["x"], -1280,
        "signed origin survives"
    );
    assert_eq!(s["screens"][1]["scale"], 1.25, "DPI scale is reported");
    // The spec asks for the serialized JSON in a text block as well, so a
    // client that ignores structuredContent still sees the answer.
    assert!(r[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\"state\":11"));
}

#[tokio::test]
async fn typing_and_chords_reach_the_wire_as_the_right_steps() {
    let (_, mock) = talk(vec![
        call(1, "type_text", json!({"text": "hi@x"})),
        call(2, "key_press", json!({"key": "enter"})),
        call(3, "key_press", json!({"key": "c", "modifiers": ["ctrl"]})),
    ])
    .await;
    let steps = mock.steps();
    let typed: Vec<String> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(typed.concat(), "hi@x", "text is injected, not keyed");
    let keys: Vec<(Key, bool)> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Key { key, down } => Some((key.clone(), *down)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            (Key::Enter, true),
            (Key::Enter, false),
            (Key::Ctrl, true),
            (Key::ch('c'), true),
            (Key::ch('c'), false),
            (Key::Ctrl, false),
        ]
    );
}

/// The spec's own distinction, honoured exactly: a caller's mistake is a
/// JSON-RPC error; the world saying no is a tool result the model can read.
#[tokio::test]
async fn caller_mistakes_are_protocol_errors() {
    let (r, _) = talk(vec![
        call(1, "no_such_tool", json!({})),
        call(2, "pointer_move", json!({"x": 5})),
        call(3, "pointer_move", json!({"x": 5, "y": 5, "screen": "NOPE"})),
        call(4, "key_press", json!({"key": "wat"})),
        call(5, "key_press", json!({"key": "a", "modifiers": ["enter"]})),
        call(6, "pointer_click", json!({"button": "sideways"})),
        json!({"jsonrpc":"2.0","id":7,"method":"nonsense"}),
    ])
    .await;
    for (i, resp) in r.iter().enumerate() {
        assert!(
            resp.get("error").is_some(),
            "#{i} should be an rpc error: {resp}"
        );
    }
    assert_eq!(r[0]["error"]["code"], -32602);
    assert_eq!(r[6]["error"]["code"], -32601, "unknown method");
    assert!(r[3]["error"]["message"].as_str().unwrap().contains("wat"));
}

#[tokio::test]
async fn a_refusal_from_the_machine_is_a_readable_tool_result() {
    // A pointer that is suspended: the person has taken their machine back.
    struct Suspended;
    #[async_trait::async_trait]
    impl nspointer::Pointer for Suspended {
        async fn screens(&self) -> Result<Screens, InputError> {
            Ok(layout())
        }
        async fn position(&self) -> Result<Point, InputError> {
            Ok(Point::new(0, 0))
        }
        async fn perform(&self, _: &[Step]) -> Result<u64, InputError> {
            Err(InputError::Agent {
                kind: ErrorKind::Suspended,
                detail: "local override, 2400ms remaining".into(),
            })
        }
    }
    let session = Session::open(Suspended).await.unwrap();
    // Existing behaviour tests run ungated; the gate has its own below.
    let server = McpServer::new(session).with_confirm(Confirm::Off);
    let (client, srv) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    tokio::spawn(async move {
        let _ = server.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();
    let mut b = serde_json::to_vec(&call(1, "pointer_click", json!({"x": 10, "y": 10}))).unwrap();
    b.push(b'\n');
    cw.write_all(&b).await.unwrap();
    let resp: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();

    // Not an rpc error: the model is the right reader for this.
    assert!(resp.get("error").is_none(), "{resp}");
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Suspended"), "{text}");
    assert!(
        text.contains("taken control back"),
        "the reason is spelled out for the reader who can act on it: {text}"
    );
}

#[tokio::test]
async fn key_names_parse_the_way_the_schema_advertises() {
    assert_eq!(parse_key("enter"), Some(Key::Enter));
    assert_eq!(parse_key("ESC"), Some(Key::Escape));
    assert_eq!(parse_key("f12"), Some(Key::F { n: 12 }));
    assert_eq!(parse_key("a"), Some(Key::ch('a')));
    assert_eq!(parse_key("cmd"), Some(Key::Meta));
    assert_eq!(parse_key("page_up"), Some(Key::PageUp));
    assert_eq!(parse_key("f99"), None);
    assert_eq!(parse_key("nonsense"), None);
}

#[tokio::test]
async fn a_malformed_line_does_not_kill_the_session() {
    let (client, srv) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    let mock = Arc::new(MockPointer::new(layout(), Point::new(0, 0)));
    struct S(Arc<MockPointer>);
    #[async_trait::async_trait]
    impl nspointer::Pointer for S {
        async fn screens(&self) -> Result<Screens, InputError> {
            self.0.screens().await
        }
        async fn position(&self) -> Result<Point, InputError> {
            self.0.position().await
        }
        async fn perform(&self, s: &[Step]) -> Result<u64, InputError> {
            self.0.perform(s).await
        }
    }
    let server = McpServer::new(Session::open(S(mock)).await.unwrap());
    tokio::spawn(async move {
        let _ = server.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();
    cw.write_all(b"{ not json\n").await.unwrap();
    let e: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(e["error"]["code"], -32700);

    let mut b = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":9,"method":"ping"})).unwrap();
    b.push(b'\n');
    cw.write_all(&b).await.unwrap();
    let ok: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ok["id"], 9, "the session survives");
}

/// Bulk text goes via the clipboard, not eight thousand keystrokes — and the
/// capability is optional, so "this agent has no clipboard" must reach the
/// model as something it can route around.
#[tokio::test]
async fn the_clipboard_round_trips_through_mcp() {
    let (r, _) = talk(vec![
        call(
            1,
            "clipboard_write",
            json!({"text": "a long pasted document"}),
        ),
        call(2, "clipboard_read", json!({})),
        call(3, "clipboard_write", json!({})),
    ])
    .await;
    assert_eq!(r[0]["result"]["structuredContent"]["chars"], 22);
    assert_eq!(
        r[1]["result"]["structuredContent"]["text"],
        "a long pasted document"
    );
    assert_eq!(
        r[2]["error"]["code"], -32602,
        "missing text is a caller bug"
    );
}

#[tokio::test]
async fn an_unsupported_capability_reads_as_something_to_route_around() {
    struct NoClipboard;
    #[async_trait::async_trait]
    impl nspointer::Pointer for NoClipboard {
        async fn screens(&self) -> Result<Screens, InputError> {
            Ok(layout())
        }
        async fn position(&self) -> Result<Point, InputError> {
            Ok(Point::new(0, 0))
        }
        async fn perform(&self, _: &[Step]) -> Result<u64, InputError> {
            Ok(0)
        }
        // clipboard_read defaults to Unsupported
    }
    let server = McpServer::new(Session::open(NoClipboard).await.unwrap());
    let (client, srv) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    tokio::spawn(async move {
        let _ = server.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();
    let mut b = serde_json::to_vec(&call(1, "clipboard_read", json!({}))).unwrap();
    b.push(b'\n');
    cw.write_all(&b).await.unwrap();
    let resp: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();

    // Not a protocol error: the model can pick another route.
    assert!(resp.get("error").is_none(), "{resp}");
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Unsupported"), "{text}");
    assert!(text.contains("use another approach"), "{text}");
}

/// The route to a click that does not involve guessing pixels: name a
/// control, get a point, click it. No screenshot anywhere in the loop.
#[tokio::test]
async fn ui_find_turns_a_name_into_a_click_target() {
    use nspointer::ui::UiNode;
    struct WithUi(std::sync::Mutex<Vec<Step>>);
    #[async_trait::async_trait]
    impl nspointer::Pointer for WithUi {
        async fn screens(&self) -> Result<Screens, InputError> {
            Ok(layout())
        }
        async fn position(&self) -> Result<Point, InputError> {
            Ok(Point::new(0, 0))
        }
        async fn perform(&self, s: &[Step]) -> Result<u64, InputError> {
            self.0.lock().unwrap().extend_from_slice(s);
            Ok(11)
        }
        async fn ui_tree(&self, _visible_only: bool) -> Result<Vec<UiNode>, InputError> {
            Ok(vec![
                UiNode {
                    role: "Group".into(),
                    name: "Toolbar".into(),
                    center: Point::new(100, 40),
                    h: 24,
                    visible: true,
                    enabled: true,
                    ..Default::default()
                },
                UiNode {
                    role: "Button".into(),
                    name: "Save".into(),
                    center: Point::new(300, 200),
                    h: 24,
                    visible: true,
                    enabled: true,
                    ..Default::default()
                },
                UiNode {
                    role: "Button".into(),
                    name: "Hidden".into(),
                    center: Point::new(9, 9),
                    h: 24,
                    visible: false,
                    enabled: true,
                    ..Default::default()
                },
            ])
        }
    }
    let server = McpServer::new(Session::open(WithUi(Default::default())).await.unwrap());
    let (client, srv) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    tokio::spawn(async move {
        let _ = server.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();
    let send = |v: Value| {
        let mut b = serde_json::to_vec(&v).unwrap();
        b.push(b'\n');
        b
    };

    cw.write_all(&send(call(1, "ui_find", json!({"name": "Save"}))))
        .await
        .unwrap();
    let r: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let m = &r["result"]["structuredContent"]["matches"][0];
    assert_eq!(m["name"], "Save");
    assert_eq!(m["x"], 300);
    assert_eq!(m["y"], 200);

    cw.write_all(&send(call(2, "ui_read", json!({}))))
        .await
        .unwrap();
    let r: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let sc = &r["result"]["structuredContent"];
    // Compression is reported rather than asserted: 3 raw, the hidden one gone.
    assert_eq!(sc["raw_controls"], 3);
    assert_eq!(sc["controls"], 2);
    assert!(sc["text"]
        .as_str()
        .unwrap()
        .contains("button \"Save\" (300,200)"));
    assert!(!sc["text"].as_str().unwrap().contains("Hidden"));
}

/// The asymmetry this closes: the harness path stages irreversible actions
/// through `SideEffectGate` and an MCP client gets nothing. A well-behaved
/// client prompts anyway, so gating every call doubles its prompts; gating
/// none trusts a property nothing checks. One explicit moment per session.
#[tokio::test]
async fn the_first_irreversible_action_must_say_it_means_it() {
    let mock = Arc::new(MockPointer::new(layout(), Point::new(0, 0)));
    let server = McpServer::new(Session::open(mock.clone()).await.unwrap());
    let (client, srv) = tokio::io::duplex(256 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    tokio::spawn(async move {
        let _ = server.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();
    let ask = |v: Value| {
        let mut b = serde_json::to_vec(&v).unwrap();
        b.push(b'\n');
        b
    };
    let next = |line: String| -> Value { serde_json::from_str(&line).unwrap() };

    // Reads and moves pass straight through: nothing is activated by looking,
    // or by a cursor arriving somewhere.
    cw.write_all(&ask(call(1, "pointer_move", json!({"x": 10, "y": 10}))))
        .await
        .unwrap();
    let r = next(lines.next_line().await.unwrap().unwrap());
    assert_eq!(r["result"]["isError"], false, "a move is not gated");

    // The first click is refused, and the refusal names what it would do.
    let before = mock.steps().len();
    cw.write_all(&ask(call(2, "pointer_click", json!({"x": 800, "y": 400}))))
        .await
        .unwrap();
    let r = next(lines.next_line().await.unwrap().unwrap());
    assert_eq!(r["result"]["isError"], true);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("click (800, 400)"),
        "names the action: {text}"
    );
    assert!(text.contains("cannot be undone"), "{text}");
    assert_eq!(
        mock.steps().len(),
        before,
        "and not one step reached the desktop"
    );

    // Saying so lets it through...
    cw.write_all(&ask(call(
        3,
        "pointer_click",
        json!({"x": 800, "y": 400, "confirm": true}),
    )))
    .await
    .unwrap();
    let r = next(lines.next_line().await.unwrap().unwrap());
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert!(mock.steps().len() > before, "the confirmed click landed");

    // ...and arms the session, so the rest is not a prompt per click.
    cw.write_all(&ask(call(4, "type_text", json!({"text": "already armed"}))))
        .await
        .unwrap();
    let r = next(lines.next_line().await.unwrap().unwrap());
    assert_eq!(r["result"]["isError"], false, "{r}");
}

#[tokio::test]
async fn every_action_mode_never_arms_and_off_never_asks() {
    for (mode, first_ok, second_ok) in [
        (Confirm::EveryAction, false, false),
        (Confirm::Off, true, true),
    ] {
        let mock = Arc::new(MockPointer::new(layout(), Point::new(0, 0)));
        let server = McpServer::new(Session::open(mock).await.unwrap()).with_confirm(mode);
        let (client, srv) = tokio::io::duplex(256 * 1024);
        let (sr, sw) = tokio::io::split(srv);
        tokio::spawn(async move {
            let _ = server.serve(sr, sw).await;
        });
        let (cr, mut cw) = tokio::io::split(client);
        let mut lines = BufReader::new(cr).lines();

        let go = |id: u64| {
            let mut b =
                serde_json::to_vec(&call(id, "key_press", json!({"key": "enter"}))).unwrap();
            b.push(b'\n');
            b
        };
        // First, with an explicit yes, then a second without one.
        let mut confirmed = serde_json::to_vec(&call(
            1,
            "key_press",
            json!({"key": "enter", "confirm": true}),
        ))
        .unwrap();
        confirmed.push(b'\n');
        cw.write_all(&confirmed).await.unwrap();
        let r: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(r["result"]["isError"], false, "{mode:?} confirmed: {r}");
        let _ = first_ok;

        cw.write_all(&go(2)).await.unwrap();
        let r: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            r["result"]["isError"], !second_ok,
            "{mode:?} does not arm across calls: {r}"
        );
    }
}

#[tokio::test]
async fn the_confirm_argument_is_advertised_on_the_tools_that_need_it() {
    let (r, _) = talk(vec![
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
    ])
    .await;
    let tools = r[0]["result"]["tools"].as_array().unwrap();
    let has_confirm = |name: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["inputSchema"]["properties"]
            .get("confirm")
            .is_some()
    };
    for t in ["pointer_click", "pointer_drag", "type_text", "key_press"] {
        assert!(has_confirm(t), "{t} should advertise confirm");
    }
    for t in ["pointer_move", "ui_read", "screens_list", "pointer_scroll"] {
        assert!(!has_confirm(t), "{t} should not");
    }
}

/// The refusal a human can lift from where they are sitting.
///
/// `needs_confirmation` exists because the other two refusals mislead here.
/// `blocked` says the OS refused and will keep refusing, and a model told that
/// reasonably stops trying something one keypress would allow; `suspended`
/// lapses on its own, so the answer is to wait. This one lapses only when a
/// person acts, so the detail — which chord, on which machine — has to survive
/// all the way to the reader, and the model has to be told not to spin.
#[tokio::test]
async fn a_refusal_awaiting_a_human_says_what_would_arm_it() {
    struct NotArmed;
    #[async_trait::async_trait]
    impl nspointer::Pointer for NotArmed {
        async fn screens(&self) -> Result<Screens, InputError> {
            Ok(layout())
        }
        async fn position(&self) -> Result<Point, InputError> {
            Ok(Point::new(0, 0))
        }
        async fn perform(&self, _: &[Step]) -> Result<u64, InputError> {
            Err(InputError::Agent {
                kind: ErrorKind::NeedsConfirmation,
                detail: "not armed. This would press the Left mouse button on a real \
                         desktop, which cannot be undone. Press ctrl+alt+p on the machine \
                         itself to arm this session."
                    .into(),
            })
        }
    }
    let session = Session::open(NotArmed).await.unwrap();
    let server = McpServer::new(session).with_confirm(Confirm::Off);
    let (client, srv) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    tokio::spawn(async move {
        let _ = server.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();
    let mut b = serde_json::to_vec(&call(1, "pointer_click", json!({"x": 10, "y": 10}))).unwrap();
    b.push(b'\n');
    cw.write_all(&b).await.unwrap();
    let resp: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();

    assert!(resp.get("error").is_none(), "{resp}");
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();

    // The instruction the agent gave has to reach the reader intact. A model
    // that is told "not armed" but not *how* to arm it can only guess.
    assert!(
        text.contains("ctrl+alt+p"),
        "the chord that would arm it must survive: {text}"
    );
    assert!(text.contains("NeedsConfirmation"), "{text}");
    assert!(
        text.contains("Do not retry in a loop"),
        "retrying cannot change the answer, so the model must be told: {text}"
    );
    // And it must not read as either of the refusals it is not.
    assert!(
        !text.contains("OS refused") && !text.contains("taken control back"),
        "must not be dressed as blocked or suspended: {text}"
    );
}

/// Drives any server over a duplex; one response per request that has an id.
async fn drive<P: nspointer::Pointer + 'static>(
    server: McpServer<P>,
    reqs: Vec<Value>,
) -> Vec<Value> {
    let (client, srv) = tokio::io::duplex(256 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    tokio::spawn(async move {
        let _ = server.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();
    let mut out = Vec::new();
    for r in reqs {
        let has_id = r.get("id").is_some();
        let mut b = serde_json::to_vec(&r).unwrap();
        b.push(b'\n');
        cw.write_all(&b).await.unwrap();
        cw.flush().await.unwrap();
        if has_id {
            let line = lines.next_line().await.unwrap().unwrap();
            out.push(serde_json::from_str(&line).unwrap());
        }
    }
    out
}

/// A gated agent: refuses commits until `armed` is set, like ns-pointerd.
struct Gated(Arc<std::sync::atomic::AtomicBool>);
#[async_trait::async_trait]
impl nspointer::Pointer for Gated {
    async fn screens(&self) -> Result<Screens, InputError> {
        Ok(layout())
    }
    async fn position(&self) -> Result<Point, InputError> {
        Ok(Point::new(0, 0))
    }
    async fn perform(&self, steps: &[Step]) -> Result<u64, InputError> {
        let commits = steps.iter().any(|s| {
            matches!(
                s,
                Step::Button { down: true, .. } | Step::Key { down: true, .. } | Step::Text { .. }
            )
        });
        if commits && !self.0.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(InputError::Agent {
                kind: ErrorKind::NeedsConfirmation,
                detail: "not armed. Press ctrl+alt+p on the machine itself to arm this session."
                    .into(),
            });
        }
        Ok(11)
    }
}

fn init() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
}

/// The promise made in the 2026-09-05 reply: once the agent fills `armed`,
/// the model hears it where it reads — `initialize`'s instructions and the
/// tool every description says to call first — rather than from the first
/// refusal. And it stays true: an accepted commit means a person armed it.
#[tokio::test]
async fn the_agents_arming_state_reaches_the_model_and_tracks_what_performs_say() {
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let session = Session::open(Gated(flag.clone())).await.unwrap();
    let server = McpServer::new(session)
        .with_confirm(Confirm::Off)
        .with_agent(true, Some(false));
    let (client, srv) = tokio::io::duplex(256 * 1024);
    let (sr, sw) = tokio::io::split(srv);
    let server = Arc::new(server);
    let s2 = server.clone();
    tokio::spawn(async move {
        let _ = s2.serve(sr, sw).await;
    });
    let (cr, mut cw) = tokio::io::split(client);
    let mut lines = BufReader::new(cr).lines();
    let ask = |r: Value| {
        let mut b = serde_json::to_vec(&r).unwrap();
        b.push(b'\n');
        b
    };
    async fn one(
        cw: &mut (impl AsyncWriteExt + Unpin),
        lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        b: Vec<u8>,
    ) -> Value {
        cw.write_all(&b).await.unwrap();
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
    }

    // Before anything: initialize says so, in words a model acts on.
    let r = one(&mut cw, &mut lines, ask(init())).await;
    let text = r["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("NOT ARMED"), "{text}");
    assert!(text.contains("needs_confirmation"), "{text}");
    assert!(text.contains("Tell the user before you start"), "{text}");
    assert!(
        !text.contains("NO LOCAL OVERRIDE"),
        "override exists here: {text}"
    );

    // And screens_list carries it structurally, with a note a model reads.
    let r = one(&mut cw, &mut lines, ask(call(2, "screens_list", json!({})))).await;
    assert_eq!(r["result"]["structuredContent"]["armed"], false, "{r}");
    assert_eq!(r["result"]["structuredContent"]["local_override"], true);
    assert!(r["result"]["structuredContent"]["note"]
        .as_str()
        .unwrap()
        .contains("Not armed"));

    // A commit is refused, the refusal names the chord, and the state holds.
    let r = one(
        &mut cw,
        &mut lines,
        ask(call(3, "pointer_click", json!({"x": 10, "y": 10}))),
    )
    .await;
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("ctrl+alt+p"));
    assert_eq!(server.agent_armed(), Some(false));

    // A person presses the chord. The next accepted commit is the evidence.
    flag.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = one(
        &mut cw,
        &mut lines,
        ask(call(4, "pointer_click", json!({"x": 10, "y": 10}))),
    )
    .await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(server.agent_armed(), Some(true));
    let r = one(&mut cw, &mut lines, ask(call(5, "screens_list", json!({})))).await;
    assert_eq!(r["result"]["structuredContent"]["armed"], true);
    assert!(
        r["result"]["structuredContent"].get("note").is_none(),
        "no note once armed: {r}"
    );

    // Disarmed again (idle expiry, the manual brake): the refusal flips it back.
    flag.store(false, std::sync::atomic::Ordering::SeqCst);
    let r = one(
        &mut cw,
        &mut lines,
        ask(call(6, "type_text", json!({"text": "x"}))),
    )
    .await;
    assert_eq!(r["result"]["isError"], true);
    assert_eq!(server.agent_armed(), Some(false));

    // A move was never gated, so it says nothing about arming either way.
    flag.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = one(
        &mut cw,
        &mut lines,
        ask(call(7, "pointer_move", json!({"x": 10, "y": 10}))),
    )
    .await;
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(
        server.agent_armed(),
        Some(false),
        "a move is not evidence of arming"
    );
}

/// An agent with no gate says nothing, and nothing must be invented for it:
/// an accepted click there is not "a person armed it".
#[tokio::test]
async fn a_gateless_agent_claims_nothing_about_arming() {
    let session = Session::open(MockPointer::new(layout(), Point::new(0, 0)))
        .await
        .unwrap();
    let server = McpServer::new(session)
        .with_confirm(Confirm::Off)
        .with_agent(true, None);
    let r = drive(
        server,
        vec![
            init(),
            call(2, "pointer_click", json!({"x": 10, "y": 10})),
            call(3, "screens_list", json!({})),
        ],
    )
    .await;
    let text = r[0]["result"]["instructions"].as_str().unwrap();
    assert!(!text.contains("ARMED") && !text.contains("armed"), "{text}");
    assert_eq!(r[1]["result"]["isError"], false);
    assert!(
        r[2]["result"]["structuredContent"].get("armed").is_none(),
        "{}",
        r[2]
    );
    assert_eq!(r[2]["result"]["structuredContent"]["local_override"], true);
}

/// The other thing `ready` says that a model should hear before acting.
#[tokio::test]
async fn a_missing_local_override_is_said_up_front_and_in_the_gate() {
    let session = Session::open(MockPointer::new(layout(), Point::new(0, 0)))
        .await
        .unwrap();
    let server = McpServer::new(session).with_agent(false, Some(false));
    let r = drive(
        server,
        vec![init(), call(2, "pointer_click", json!({"x": 10, "y": 10}))],
    )
    .await;
    let text = r[0]["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("NO LOCAL OVERRIDE"), "{text}");
    // This server's own gate mentions the machine's gate too, so the model
    // does not learn about the second one from a second refusal.
    let gate = r[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(gate.contains("\"confirm\": true"), "{gate}");
    assert!(
        gate.contains("machine itself is also not yet armed"),
        "{gate}"
    );
    // A server built over a double with nothing declared claims nothing.
    let session = Session::open(MockPointer::new(layout(), Point::new(0, 0)))
        .await
        .unwrap();
    let r = drive(
        McpServer::new(session),
        vec![init(), call(2, "screens_list", json!({}))],
    )
    .await;
    let text = r[0]["result"]["instructions"].as_str().unwrap();
    assert!(
        !text.contains("OVERRIDE") && !text.contains("ARMED"),
        "{text}"
    );
    assert!(r[1]["result"]["structuredContent"]
        .get("local_override")
        .is_none());
}

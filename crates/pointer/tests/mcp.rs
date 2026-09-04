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
        async fn ui_tree(&self) -> Result<Vec<UiNode>, InputError> {
            Ok(vec![
                UiNode {
                    role: "Group".into(),
                    name: "Toolbar".into(),
                    center: Point::new(100, 40),
                    h: 24,
                    visible: true,
                    enabled: true,
                },
                UiNode {
                    role: "Button".into(),
                    name: "Save".into(),
                    center: Point::new(300, 200),
                    h: 24,
                    visible: true,
                    enabled: true,
                },
                UiNode {
                    role: "Button".into(),
                    name: "Hidden".into(),
                    center: Point::new(9, 9),
                    h: 24,
                    visible: false,
                    enabled: true,
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

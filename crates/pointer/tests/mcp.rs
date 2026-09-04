//! The MCP surface, driven the way a client drives it: JSON-RPC lines in,
//! JSON-RPC lines out, over an in-memory pipe.

use nspointer::mcp::{parse_key, McpServer, MCP_PROTOCOL};
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
    }
    let session = Session::open(Shared(mock.clone())).await.unwrap();
    let server = McpServer::new(session);

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
    let server = McpServer::new(session);
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

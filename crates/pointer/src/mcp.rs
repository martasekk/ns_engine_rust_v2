//! MCP server: JSON-RPC 2.0 over stdio, per the 2025-06-18 specification.
//!
//! Hand-rolled rather than taken from an SDK, for the reason the LLM client
//! in this workspace was: the surface is `initialize`, `tools/list`,
//! `tools/call` and `ping`, and owning it keeps the error mapping — which is
//! the part that actually matters here — explicit.
//!
//! **Coordinates are absolute pixels by default.** Every comparable server
//! takes them, and the canonical computer-use tool documents `coordinate:
//! [x, y]` as "pixels from the left edge", so that is the shape a model
//! reaches for. Passing `screen` switches `x`/`y` to 0.0–1.0 within that
//! screen, which is the precise form for a caller that has read
//! `screens_list`. One optional argument, not two schemas.

use crate::wire::{ErrorKind, InputError, Key};
use crate::{Button, Loc, Pointer, Session};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

pub const MCP_PROTOCOL: &str = "2025-06-18";

/// Named keys accepted by `key_press`. Anything else of length one is taken
/// as `Key::Char` — the key that produces that character on the target's
/// layout, which is what a chord needs.
pub fn parse_key(s: &str) -> Option<Key> {
    let k = s.trim().to_ascii_lowercase();
    Some(match k.as_str() {
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" => Key::Insert,
        "space" => Key::Space,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        "ctrl" | "control" => Key::Ctrl,
        "alt" => Key::Alt,
        "shift" => Key::Shift,
        "meta" | "win" | "cmd" | "super" => Key::Meta,
        "ctrl_right" => Key::CtrlRight,
        "alt_right" | "altgr" => Key::AltRight,
        "shift_right" => Key::ShiftRight,
        other => {
            if let Some(n) = other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                if (1..=24).contains(&n) {
                    return Some(Key::F { n });
                }
                return None;
            }
            let mut chars = other.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Char { c }
        }
    })
}

fn button_of(v: Option<&str>) -> Option<Button> {
    Some(match v.unwrap_or("left") {
        "left" => Button::Left,
        "right" => Button::Right,
        "middle" => Button::Middle,
        _ => return None,
    })
}

fn tools() -> Value {
    let xy = |what: &str| {
        json!({
            "x": {"type": "number", "description":
                format!("{what} X. Absolute pixel from the left of the virtual desktop, \
                         or 0.0-1.0 if `screen` is given.")},
            "y": {"type": "number", "description":
                format!("{what} Y. Absolute pixel from the top of the virtual desktop, \
                         or 0.0-1.0 if `screen` is given.")},
        })
    };
    let screen = json!({"type": "string", "description":
        "Optional screen id from screens_list. When given, x and y are fractions \
         0.0-1.0 within that screen instead of absolute pixels."});
    let button = json!({"type": "string", "enum": ["left", "right", "middle"],
                        "description": "Default left."});
    json!([
        {
            "name": "screens_list",
            "title": "List screens",
            "description": "The target's displays: stable id, pixel bounds in virtual-desktop \
                            coordinates (origins may be negative), DPI scale, and which is \
                            primary. Call this first — it is what makes coordinates mean \
                            anything, and `scale` is what lets a screenshot taken elsewhere \
                            be mapped onto this layout.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "pointer_position",
            "title": "Pointer position",
            "description": "Where the pointer is now, in absolute virtual-desktop pixels.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "pointer_move",
            "title": "Move the pointer",
            "description": "Move the pointer along a human-like path. Does not click.",
            "inputSchema": {"type": "object", "properties":
                {"x": xy("Target")["x"], "y": xy("Target")["y"], "screen": screen},
             "required": ["x", "y"]},
        },
        {
            "name": "pointer_click",
            "title": "Click",
            "description": "Move to a point and click it. Omit x and y to click where the \
                            pointer already is.",
            "inputSchema": {"type": "object", "properties": {
                "x": xy("Target")["x"], "y": xy("Target")["y"], "screen": screen,
                "button": button,
                "count": {"type": "integer", "description": "1 for a click, 2 to double-click."},
            }},
        },
        {
            "name": "pointer_drag",
            "title": "Drag",
            "description": "Press at one point, move, release at another.",
            "inputSchema": {"type": "object", "properties": {
                "from_x": {"type": "number"}, "from_y": {"type": "number"},
                "to_x": {"type": "number"}, "to_y": {"type": "number"},
                "screen": screen, "button": button,
            }, "required": ["from_x", "from_y", "to_x", "to_y"]},
        },
        {
            "name": "pointer_scroll",
            "title": "Scroll",
            "description": "Scroll in notches, not pixels. Positive dy scrolls down.",
            "inputSchema": {"type": "object", "properties": {
                "dx": {"type": "integer"}, "dy": {"type": "integer"},
            }, "required": ["dy"]},
        },
        {
            "name": "type_text",
            "title": "Type text",
            "description": "Type literal characters into whatever has focus. \
                            Layout-independent: use this for addresses, paths and prose, \
                            including characters like @ that depend on keyboard layout. \
                            It cannot press Enter or produce a shortcut — use key_press.",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string"},
            }, "required": ["text"]},
        },
        {
            "name": "clipboard_read",
            "title": "Read the clipboard",
            "description": "Read the remote machine's clipboard as text. Combined with \
                            ctrl+a then ctrl+c, this reads the contents of a text field or \
                            document without capturing the screen. May answer unsupported.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "clipboard_write",
            "title": "Set the clipboard",
            "description": "Replace the remote machine's clipboard, then paste with \
                            key_press ctrl+v. Prefer this to type_text for anything long: \
                            typing is per-character. May answer unsupported.",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string"},
            }, "required": ["text"]},
        },
        {
            "name": "key_press",
            "title": "Press a key or chord",
            "description": "Press one key, optionally with modifiers held: Enter, Tab, F5, \
                            arrows, or a shortcut like ctrl+c. Use type_text for characters \
                            you want typed.",
            "inputSchema": {"type": "object", "properties": {
                "key": {"type": "string", "description":
                    "enter tab escape backspace delete insert space up down left right \
                     home end pageup pagedown f1-f24, or a single character."},
                "modifiers": {"type": "array", "items": {"type": "string"}, "description":
                    "Held while the key is tapped, released in reverse: ctrl alt shift meta."},
            }, "required": ["key"]},
        },
    ])
}

pub struct McpServer<P: Pointer> {
    session: Mutex<Session<P>>,
}

impl<P: Pointer> McpServer<P> {
    pub fn new(session: Session<P>) -> Self {
        Self {
            session: Mutex::new(session),
        }
    }

    pub async fn serve<R, W>(&self, read: R, write: W) -> std::io::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(read).lines();
        let mut out = write;
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    write_msg(&mut out, &rpc_error(Value::Null, -32700, &e.to_string())).await?;
                    continue;
                }
            };
            // A notification has no id and takes no reply — answering one is
            // a protocol violation, not merely noise.
            let Some(id) = req.get("id").cloned() else {
                continue;
            };
            let method = req["method"].as_str().unwrap_or_default().to_string();
            let params = req.get("params").cloned().unwrap_or(json!({}));
            let resp = self.handle(id, &method, params).await;
            write_msg(&mut out, &resp).await?;
        }
        Ok(())
    }

    async fn handle(&self, id: Value, method: &str, params: Value) -> Value {
        match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": MCP_PROTOCOL,
                    // No listChanged: this set is fixed for the process.
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": "ns-pointer",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }
            }),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({
                "jsonrpc": "2.0", "id": id, "result": {"tools": tools()}
            }),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default().to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                self.call(id, &name, args).await
            }
            other => rpc_error(id, -32601, &format!("unknown method: {other}")),
        }
    }

    /// The error mapping, which is the substance of this layer.
    ///
    /// The spec draws the line between a **protocol** error and a **tool
    /// execution** error, and the distinction is worth honouring exactly: an
    /// unknown tool or an unparseable argument is a bug in the caller and
    /// belongs in `error`, while a refusal from the machine — suspended,
    /// blocked, rate-limited — is a fact about the world that the model needs
    /// to read and act on. Burying "the user has taken their machine back" in
    /// a JSON-RPC error code hides it from the one reader who could respond
    /// to it sensibly.
    async fn call(&self, id: Value, name: &str, args: Value) -> Value {
        macro_rules! bad {
            ($($t:tt)*) => { return rpc_error(id, -32602, &format!($($t)*)) };
        }
        let num = |k: &str| args.get(k).and_then(Value::as_f64);
        let screen = args
            .get("screen")
            .and_then(Value::as_str)
            .map(str::to_string);

        // Absolute pixels unless a screen is named, in which case fractions.
        let loc = |x: f64, y: f64| match &screen {
            Some(s) => Loc::normalized(s.as_str(), x, y),
            None => Loc::absolute(x.round() as i32, y.round() as i32),
        };

        let session = self.session.lock().await;
        let outcome: Result<Value, InputError> = match name {
            "screens_list" => {
                drop(session);
                let mut s = self.session.lock().await;
                match s.refresh().await {
                    Ok(()) => {
                        let sc = s.screens();
                        Ok(json!({
                            "screens": sc.screens.iter().map(|x| json!({
                                "id": x.id.0, "bounds": x.bounds, "scale": x.scale,
                                "primary": x.primary, "label": x.label,
                            })).collect::<Vec<_>>(),
                            "state": sc.state,
                        }))
                    }
                    Err(e) => Err(e),
                }
            }
            "pointer_position" => match session.position().await {
                Ok(p) => Ok(json!({"x": p.x, "y": p.y})),
                Err(e) => Err(e),
            },
            "pointer_move" => {
                let (Some(x), Some(y)) = (num("x"), num("y")) else {
                    bad!("pointer_move needs numeric x and y")
                };
                session
                    .move_to(&loc(x, y))
                    .await
                    .map(|s| json!({"state": s}))
            }
            "pointer_click" => {
                let Some(button) = button_of(args.get("button").and_then(Value::as_str)) else {
                    bad!("button must be left, right or middle")
                };
                let count = args.get("count").and_then(Value::as_u64).unwrap_or(1) as u32;
                match (num("x"), num("y")) {
                    (Some(x), Some(y)) => session.click_at(&loc(x, y), button, count).await,
                    (None, None) => session.click_here(button, count).await,
                    _ => bad!("pointer_click needs both x and y, or neither"),
                }
                .map(|s| json!({"state": s}))
            }
            "pointer_drag" => {
                let (Some(fx), Some(fy), Some(tx), Some(ty)) =
                    (num("from_x"), num("from_y"), num("to_x"), num("to_y"))
                else {
                    bad!("pointer_drag needs numeric from_x, from_y, to_x, to_y")
                };
                let Some(button) = button_of(args.get("button").and_then(Value::as_str)) else {
                    bad!("button must be left, right or middle")
                };
                session
                    .drag(&loc(fx, fy), &loc(tx, ty), button)
                    .await
                    .map(|s| json!({"state": s}))
            }
            "pointer_scroll" => {
                let dx = args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;
                let Some(dy) = args.get("dy").and_then(Value::as_i64) else {
                    bad!("pointer_scroll needs dy")
                };
                session
                    .scroll(dx, dy as i32)
                    .await
                    .map(|s| json!({"state": s}))
            }
            "clipboard_read" => session
                .clipboard_read()
                .await
                .map(|text| json!({"text": text})),
            "clipboard_write" => {
                let Some(text) = args.get("text").and_then(Value::as_str) else {
                    bad!("clipboard_write needs text")
                };
                session
                    .clipboard_write(text)
                    .await
                    .map(|()| json!({"chars": text.chars().count()}))
            }
            "type_text" => {
                let Some(text) = args.get("text").and_then(Value::as_str) else {
                    bad!("type_text needs text")
                };
                session.type_text(text).await.map(|s| json!({"state": s}))
            }
            "key_press" => {
                let Some(k) = args.get("key").and_then(Value::as_str) else {
                    bad!("key_press needs key")
                };
                let Some(key) = parse_key(k) else {
                    bad!("unknown key: {k}")
                };
                let mut mods = Vec::new();
                for m in args
                    .get("modifiers")
                    .and_then(Value::as_array)
                    .unwrap_or(&vec![])
                {
                    let Some(mk) = m.as_str().and_then(parse_key) else {
                        bad!("unknown modifier: {m}")
                    };
                    if !mk.is_modifier() {
                        bad!("{m} is not a modifier")
                    }
                    mods.push(mk);
                }
                if mods.is_empty() {
                    session.press(key).await
                } else {
                    session.chord(&mods, key).await
                }
                .map(|s| json!({"state": s}))
            }
            other => return rpc_error(id, -32602, &format!("unknown tool: {other}")),
        };

        match outcome {
            Ok(v) => tool_ok(id, v),
            // A location that does not exist is the caller's mistake.
            Err(InputError::NoSuchLocation(d)) => rpc_error(id, -32602, &d),
            // Everything else is the world saying no, and the model is the
            // right reader for it.
            Err(e) => {
                let hint = match &e {
                    InputError::Agent {
                        kind: ErrorKind::Suspended,
                        ..
                    } => " The person at the machine has taken control back; wait and retry.",
                    InputError::Agent {
                        kind: ErrorKind::Blocked,
                        ..
                    } => " The OS refused the input — an elevated window or the lock screen.",
                    InputError::Agent {
                        kind: ErrorKind::OutOfBounds,
                        ..
                    } => " The display layout changed; call screens_list again.",
                    InputError::Agent {
                        kind: ErrorKind::Unsupported,
                        ..
                    } => " This agent does not implement that; use another approach.",
                    _ => "",
                };
                tool_err(id, &format!("{e}.{hint}"))
            }
        }
    }
}

async fn write_msg<W: AsyncWrite + Unpin>(out: &mut W, v: &Value) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(v)?;
    buf.push(b'\n');
    out.write_all(&buf).await?;
    out.flush().await
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Structured content plus the serialized JSON as text, which the spec asks
/// for so a client that ignores `structuredContent` still sees the answer.
fn tool_ok(id: Value, v: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": [{"type": "text", "text": v.to_string()}],
        "structuredContent": v,
        "isError": false,
    }})
}

fn tool_err(id: Value, text: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": [{"type": "text", "text": text}],
        "isError": true,
    }})
}

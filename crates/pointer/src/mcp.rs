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
    let confirm = json!({"type": "boolean", "description":
        "Say true to confirm that the person you are working for wants input \
         put on a real desktop. Required once per session by default."});
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
                "confirm": confirm.clone(),
            }},
        },
        {
            "name": "pointer_drag",
            "title": "Drag",
            "description": "Press at one point, move, release at another.",
            "inputSchema": {"type": "object", "properties": {
                "from_x": {"type": "number"}, "from_y": {"type": "number"},
                "to_x": {"type": "number"}, "to_y": {"type": "number"},
                "screen": screen, "button": button, "confirm": confirm.clone(),
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
                "text": {"type": "string"}, "confirm": confirm.clone(),
            }, "required": ["text"]},
        },
        {
            "name": "ui_read",
            "title": "Read the UI",
            "description": "The remote machine's controls as text — role, name and a \
                            clickable point each, compressed. Prefer this to guessing \
                            coordinates: naming a control is exact, and it needs no \
                            screenshot. Anything blocking the screen is listed first under \
                            MODAL. Pass `query` to keep text windows around what you are \
                            looking for; it never removes controls. May answer unsupported.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description":
                    "What you are looking for. Steers text truncation only."},
            }},
        },
        {
            "name": "ui_find",
            "title": "Find a control",
            "description": "Controls whose name or role matches, best first, each with the \
                            point to click. The intended route to a click: ui_find \"Save\", \
                            then pointer_click at the point it returns.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"},
            }, "required": ["name"]},
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
                "confirm": confirm.clone(),
            }, "required": ["key"]},
        },
    ])
}

/// When an MCP caller must say it means it before input reaches the desktop.
///
/// The harness path stages `pointer_click` and `pointer_type` through
/// `SideEffectGate` and names the coordinates before anything happens
/// (plan §12). An MCP client gets none of that: the specification asks
/// *clients* to keep a human in the loop and cannot enforce it, and the
/// survey found no server that does.
///
/// A well-behaved client already prompts, so gating every call would double
/// the prompts it shows. Gating none of them trusts a property nothing
/// checks. `FirstAction` is the default because it costs exactly one extra
/// round trip per session and guarantees one explicit human moment even when
/// the client auto-approves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Confirm {
    Off,
    /// The first irreversible action of a session must carry `confirm: true`.
    FirstAction,
    /// Every one must.
    EveryAction,
}

pub struct McpServer<P: Pointer> {
    session: Mutex<Session<P>>,
    confirm: Confirm,
    /// This server's own gate: whether the caller has said yes once.
    armed: std::sync::atomic::AtomicBool,
    /// The agent's gate, as it last reported it (protocol §4.7). Seeded from
    /// `ready` at connect, then kept current from what performs come back
    /// with: an accepted commit means a person armed it, `needs_confirmation`
    /// means they have not. `None` is an agent with no gate, or an older one,
    /// and is never read as either answer.
    agent_armed: std::sync::Mutex<Option<bool>>,
    /// Whether the agent said a person at the machine can interrupt it.
    /// `None` until the binary tells us; a test double has no `ready`.
    local_override: Option<bool>,
}

/// Tools that put input on the desktop. A read is not one of them, and
/// neither is a move: nothing is activated by a cursor arriving somewhere.
fn irreversible(tool: &str) -> bool {
    matches!(
        tool,
        "pointer_click" | "pointer_drag" | "type_text" | "key_press"
    )
}

impl<P: Pointer> McpServer<P> {
    pub fn new(session: Session<P>) -> Self {
        Self {
            session: Mutex::new(session),
            confirm: Confirm::FirstAction,
            armed: std::sync::atomic::AtomicBool::new(false),
            agent_armed: std::sync::Mutex::new(None),
            local_override: None,
        }
    }

    pub fn with_confirm(mut self, confirm: Confirm) -> Self {
        self.confirm = confirm;
        self
    }

    /// What the agent said in `ready`. The binary reads both off
    /// `RemotePointer` before the session takes it; a caller building over a
    /// double can leave them unset and nothing is claimed either way.
    pub fn with_agent(self, local_override: bool, armed: Option<bool>) -> Self {
        *self.agent_armed.lock().unwrap() = armed;
        Self {
            local_override: Some(local_override),
            ..self
        }
    }

    /// The agent's arming state as this server currently understands it.
    pub fn agent_armed(&self) -> Option<bool> {
        *self.agent_armed.lock().unwrap()
    }

    /// Learn from what a commit came back with. Only an agent that has
    /// declared a gate is tracked: an accepted click on a gateless agent says
    /// nothing about arming, and `Some(true)` there would be an invention.
    fn observe(&self, name: &str, outcome: &Result<Value, InputError>) {
        if !irreversible(name) {
            return;
        }
        let mut agent = self.agent_armed.lock().unwrap();
        match outcome {
            Err(InputError::Agent {
                kind: ErrorKind::NeedsConfirmation,
                ..
            }) => *agent = Some(false),
            Ok(_) if agent.is_some() => *agent = Some(true),
            _ => {}
        }
    }

    /// The `instructions` a client hands its model at `initialize` — the one
    /// place the spec gives a server to say how it should be used, and the
    /// place `armed` belongs: a model told at connect that the first click
    /// will be refused until a person presses a chord can ask for that up
    /// front, instead of discovering it from the refusal.
    fn instructions(&self) -> String {
        let mut s = String::from(
            "Drives the pointer and keyboard of a real desktop on another machine. \
             Call screens_list first. To click something, prefer ui_find (or ui_read) \
             to name the control, then pointer_click at the point it returns; guess \
             coordinates only from a screenshot mapped with the scale screens_list \
             gives. type_text types characters; key_press presses Enter, Tab and \
             shortcuts; for long text, clipboard_write then key_press ctrl+v. A \
             refusal from the machine arrives as a tool result, not a protocol error: \
             read it, because it says what to do next.",
        );
        match self.agent_armed() {
            Some(false) => s.push_str(
                " The agent reports this session is NOT ARMED: a person at that machine \
                 must press its arming chord before the first click, key or text is \
                 accepted, and until then those come back needs_confirmation naming the \
                 chord. Tell the user before you start, and do not retry in a loop.",
            ),
            Some(true) => s.push_str(" A person at that machine has armed this session for input."),
            None => {}
        }
        if self.local_override == Some(false) {
            s.push_str(
                " There is NO LOCAL OVERRIDE on that machine: its user cannot interrupt \
                 this session by touching the mouse or keyboard. Act conservatively and \
                 stop at the first sign that something unexpected has focus.",
            );
        }
        s
    }

    /// Whether this call may proceed, or the sentence explaining what it
    /// would do and how to say yes. Named specifically, because "confirm
    /// 'pointer_click'" is not a question anyone can answer.
    fn gate(&self, name: &str, args: &Value) -> Option<String> {
        use std::sync::atomic::Ordering;
        if self.confirm == Confirm::Off || !irreversible(name) {
            return None;
        }
        let said_yes = args
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if said_yes {
            self.armed.store(true, Ordering::SeqCst);
            return None;
        }
        if self.confirm == Confirm::FirstAction && self.armed.load(Ordering::SeqCst) {
            return None;
        }
        let what = match name {
            "pointer_click" => match (args.get("x"), args.get("y")) {
                (Some(x), Some(y)) => format!("click ({x}, {y})"),
                _ => "click where the pointer is".into(),
            },
            "pointer_drag" => format!(
                "drag ({}, {}) to ({}, {})",
                args["from_x"], args["from_y"], args["to_x"], args["to_y"]
            ),
            "type_text" => format!(
                "type {:?}",
                args.get("text").and_then(Value::as_str).unwrap_or("")
            ),
            "key_press" => format!(
                "press {}",
                args.get("key").and_then(Value::as_str).unwrap_or("?")
            ),
            _ => name.to_string(),
        };
        let scope = if self.confirm == Confirm::FirstAction {
            " Confirming once arms this session for the rest of its input."
        } else {
            ""
        };
        // Two gates, two people. Saying so here saves the model a round trip
        // it would otherwise spend learning it from the agent's refusal.
        let machine = if self.agent_armed() == Some(false) {
            " The machine itself is also not yet armed: a person there must \
             press its arming chord as well."
        } else {
            ""
        };
        Some(format!(
            "This would {what} on a real desktop, which cannot be undone. \
             Call again with \"confirm\": true if the person you are working for \
             wants that.{scope}{machine}"
        ))
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
                    "instructions": self.instructions(),
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

        if let Some(ask) = self.gate(name, &args) {
            return tool_err(id, &ask);
        }

        let session = self.session.lock().await;
        let outcome: Result<Value, InputError> = match name {
            "screens_list" => {
                drop(session);
                let mut s = self.session.lock().await;
                match s.refresh().await {
                    Ok(()) => {
                        let sc = s.screens();
                        let mut v = json!({
                            "screens": sc.screens.iter().map(|x| json!({
                                "id": x.id.0, "bounds": x.bounds, "scale": x.scale,
                                "primary": x.primary, "label": x.label,
                            })).collect::<Vec<_>>(),
                            "state": sc.state,
                        });
                        // The tool every description says to call first is
                        // the other place a model will read this. Present
                        // only when the agent has a gate, so absence is not
                        // mistaken for "armed".
                        if let Some(armed) = self.agent_armed() {
                            v["armed"] = json!(armed);
                            if !armed {
                                v["note"] = json!(
                                    "Not armed: a person at the machine must press the \
                                     arming chord before the first click, key or text \
                                     is accepted."
                                );
                            }
                        }
                        if let Some(lo) = self.local_override {
                            v["local_override"] = json!(lo);
                        }
                        Ok(v)
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
            "ui_read" => {
                let q = args.get("query").and_then(Value::as_str);
                session.ui_read(q).await.map(|v| {
                    json!({
                        "text": v.render(),
                        "controls": v.nodes.len(),
                        "modals": v.modals.len(),
                        "raw_controls": v.raw_count,
                    })
                })
            }
            "ui_find" => {
                let Some(name) = args.get("name").and_then(Value::as_str) else {
                    bad!("ui_find needs name")
                };
                session.ui_read(Some(name)).await.map(|v| {
                    json!({
                        "matches": v.find(name).iter().take(10).map(|n| json!({
                            "role": n.role, "name": n.name,
                            "x": n.center.x, "y": n.center.y,
                        })).collect::<Vec<_>>()
                    })
                })
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
        self.observe(name, &outcome);

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
                    // The one refusal a human can lift from where they are
                    // sitting, so it is the one worth spending a sentence on.
                    // The agent's own detail names the chord and the machine;
                    // repeating the call cannot change the answer, and a model
                    // that loops here just burns the session in silence.
                    InputError::Agent {
                        kind: ErrorKind::NeedsConfirmation,
                        ..
                    } => {
                        " A person at that machine has to approve this first. \
                          Do not retry in a loop: tell the user what you are about \
                          to do and what the error says will arm it."
                    }
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

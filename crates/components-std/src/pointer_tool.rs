//! Remote pointer and keyboard as harness actions, so `ns-app`'s own emitter
//! can drive a desktop the way it calls `get_time`.
//!
//! The reason to do this rather than only ship the MCP server: the engine
//! already has the machinery this capability needs and an MCP client does
//! not. A click on someone else's desktop is `SideEffect::Irreversible`, so
//! `SideEffectGate` stages it and the user is asked before it happens —
//! naming the exact coordinates, because "confirm 'pointer_click'" is not a
//! question anyone can answer. Provenance, the repeat gate and the event log
//! come along for free.
//!
//! Classification is the substance here:
//!
//! | action | effect | why |
//! | --- | --- | --- |
//! | `pointer_screens`, `pointer_position` | `Pure` | reads |
//! | `pointer_move`, `pointer_scroll` | `Reversible` | moves a cursor; activates nothing |
//! | `pointer_click`, `pointer_type` | `Irreversible` | **you cannot know what a click activated** — there is no undo for "sent the email" |

use async_trait::async_trait;
use nscore::{
    ActionSpec, SchemaProfile, SideEffect, StagedEffect, Tool, ToolCtx, ToolError, ToolOutput,
    Trust,
};
use nspointer::backoff::WaitOutOverride;
use nspointer::mcp::parse_key;
use nspointer::{Button, Loc, Pointer, Session};
use std::sync::Arc;
use tokio::sync::Mutex;

/// One connection, shared by every action. A `Session` caches the screen
/// layout, and two tools disagreeing about it would resolve the same
/// coordinates differently.
type Shared = Arc<Mutex<Session<Arc<dyn Pointer>>>>;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Act {
    Screens,
    Position,
    Move,
    Click,
    Scroll,
    Type,
    ClipRead,
    ClipWrite,
    UiRead,
    UiFind,
}

pub struct PointerTool {
    spec: ActionSpec,
    act: Act,
    shared: Shared,
}

/// Every action over one connection. Register the lot with `HarnessBuilder`.
pub async fn tools(
    pointer: Arc<dyn Pointer>,
    profile: SchemaProfile,
) -> Result<Vec<Arc<dyn Tool>>, String> {
    // Wrapped here rather than at each call site: the local override is
    // re-armed by every attempt, so a model that retries a suspended
    // `perform` can never get through and simply loops. `WaitOutOverride`
    // waits the time the agent itself reports and tries exactly once more.
    let pointer: Arc<dyn Pointer> = Arc::new(WaitOutOverride::new(pointer));
    let session = Session::open(pointer).await.map_err(|e| e.to_string())?;
    let shared: Shared = Arc::new(Mutex::new(session));
    Ok([
        Act::Screens,
        Act::Position,
        Act::Move,
        Act::Click,
        Act::Scroll,
        Act::Type,
        Act::ClipRead,
        Act::ClipWrite,
        Act::UiRead,
        Act::UiFind,
    ]
    .into_iter()
    .map(|act| Arc::new(PointerTool::new(act, shared.clone(), profile)) as Arc<dyn Tool>)
    .collect())
}

/// The coordinate convention, written once (M10 T1.2).
///
/// It used to be spelled out per axis, three times per tool and twice over
/// (`pointer_click` and `pointer_move`): ~280 chars of the same sentence
/// duplicated, and the two tools were the array's top carriers at 218 and
/// 174 tokens (findings §8.1). The convention is a property of the tool, not
/// of `x` separately from `y`, so it belongs in the tool's description; what
/// stays on each axis is the one phrase that distinguishes it from the other.
const COORDS: &str =
    "x and y are desktop pixels from the top-left, or 0.0-1.0 fractions of `screen`.";

/// The same sentence, and it has to be the same sentence — the emitter that
/// learns the convention from `pointer_move` uses it on `pointer_click`.
const COORDS_SLIM: &str = "x,y: desktop pixels from top-left, or 0-1 of `screen`.";

fn coords(profile: SchemaProfile) -> &'static str {
    profile.pick(COORDS, COORDS_SLIM)
}

fn xy_schema(extra: serde_json::Value) -> serde_json::Value {
    let mut base = serde_json::json!({
        "type": "object",
        "properties": {
            "x": {"type": "number", "description": "from the left"},
            "y": {"type": "number", "description": "from the top"},
            "screen": {"type": "string", "description": "id from pointer_screens"},
            // NB: `screen` keeps its phrase because its *name* does not say
            // where the id comes from; `x` and `y` get one word each and the
            // convention itself lives in the tool's description.
        },
        "required": ["x", "y"]
    });
    if let (Some(p), Some(e)) = (base["properties"].as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    base
}

/// Every desktop action, as specs, with no connection and no daemon (M10
/// T0.1). `ns-app budget` prices a recorded tool array from the names in the
/// manifest, and it has to do that on a box where the pointer daemon is not
/// running — the log is the measurement, not the machine.
pub fn specs(profile: SchemaProfile) -> Vec<ActionSpec> {
    [
        Act::Screens,
        Act::Position,
        Act::Move,
        Act::Click,
        Act::Scroll,
        Act::Type,
        Act::ClipRead,
        Act::ClipWrite,
        Act::UiRead,
        Act::UiFind,
    ]
    .into_iter()
    .map(|a| spec_of(a, profile))
    .collect()
}

/// The one place an action's name, description and argument schema are
/// written. Split out of `PointerTool::new` so a spec can be had without a
/// session; `new` still goes through it, so the two cannot drift.
fn spec_of(act: Act, profile: SchemaProfile) -> ActionSpec {
    let coords = coords(profile);
    // `middle` was never once proposed in the recorded 21 turns, and neither
    // was a `modifiers` array outside ctrl; `slim` collapses the enum to the
    // two values the log used. The *parameter* stays in both profiles — the
    // plan's rule is that a removed parameter is a removed capability.
    let buttons: serde_json::Value = profile.pick(
        serde_json::json!(["left", "right", "middle"]),
        serde_json::json!(["left", "right"]),
    );
    let (name, description, side_effect, args_schema) = match act {
        Act::Screens => (
            "pointer_screens",
            profile.pick(
                "List the remote machine's displays: id, pixel bounds, DPI scale, which is \
                 primary. Call before using absolute coordinates."
                    .to_string(),
                "List the displays: id, bounds, DPI, primary.".to_string(),
            ),
            SideEffect::Pure,
            serde_json::json!({"type": "object", "properties": {}}),
        ),
        Act::Position => (
            "pointer_position",
            profile.pick(
                "Where the remote pointer is now.".to_string(),
                "Report where the pointer is.".to_string(),
            ),
            SideEffect::Pure,
            serde_json::json!({"type": "object", "properties": {}}),
        ),
        Act::Move => (
            "pointer_move",
            // The argument example (M10 T1.5): the recorded log's two
            // `IllegalAction` rejections both name `pointer_move`, and its
            // one `Malformed` sibling on `pointer_click` was a missing `x`.
            // One example each, in both profiles.
            format!("Move the pointer, without clicking. {coords} Ex: {{\"x\":640,\"y\":400}}."),
            SideEffect::Reversible,
            xy_schema(serde_json::json!({})),
        ),
        Act::Click => (
            "pointer_click",
            format!(
                "Click a point, activating whatever is under it. {coords} \
                 Ex: {{\"x\":640,\"y\":400}}."
            ),
            SideEffect::Irreversible,
            xy_schema(serde_json::json!({
                "button": {"type": "string", "enum": buttons},
                "count": {"type": "integer", "description": "2 to double-click"},
            })),
        ),
        Act::Scroll => (
            "pointer_scroll",
            profile.pick(
                "Scroll the remote machine in notches. Positive dy scrolls down.".to_string(),
                "Scroll in notches; positive dy scrolls down.".to_string(),
            ),
            SideEffect::Reversible,
            serde_json::json!({
                "type": "object",
                "properties": {"dx": {"type": "integer"}, "dy": {"type": "integer"}},
                "required": ["dy"]
            }),
        ),
        Act::Type => (
            "pointer_type",
            profile.pick(
                "Type text, or press a key, on the remote machine. Irreversible: it goes to \
                 whatever has focus. Use `text` for characters and `key` (with optional \
                 `modifiers`) for Enter, F5 or a shortcut like ctrl+c."
                    .to_string(),
                "Send keystrokes to whatever has focus: `text` for characters, `key` with \
                 optional `modifiers` for Enter, F5 or ctrl+c."
                    .to_string(),
            ),
            SideEffect::Irreversible,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "key": {"type": "string"},
                    "modifiers": {"type": "array", "items": {"type": "string"}},
                }
            }),
        ),
        Act::ClipRead => (
            "pointer_clipboard_read",
            profile.pick(
                "Read the remote machine's clipboard. With ctrl+a then ctrl+c, this reads a \
                 text field or document without a screenshot."
                    .to_string(),
                "Read the clipboard; after ctrl+a ctrl+c it reads a field without a \
                 screenshot."
                    .to_string(),
            ),
            SideEffect::Pure,
            serde_json::json!({"type": "object", "properties": {}}),
        ),
        Act::ClipWrite => (
            "pointer_clipboard_write",
            profile.pick(
                "Replace the remote machine's clipboard, then paste it with pointer_type \
                 key=v modifiers=[ctrl]. Prefer this to typing anything long."
                    .to_string(),
                "Replace the clipboard, then paste with pointer_type key=v \
                 modifiers=[ctrl]."
                    .to_string(),
            ),
            SideEffect::Reversible,
            serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
        ),
        Act::UiRead => (
            "pointer_ui_read",
            profile.pick(
                "The remote machine's controls as text — role, name and a clickable point \
                 each. Prefer this to guessing coordinates. Anything blocking the screen is \
                 listed first under MODAL."
                    .to_string(),
                "List the on-screen controls as role, name and a clickable point; anything \
                 blocking comes first under MODAL."
                    .to_string(),
            ),
            SideEffect::Pure,
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }),
        ),
        Act::UiFind => (
            "pointer_ui_find",
            profile.pick(
                "Find a control by name and get the point to click. The route to a click \
                 that does not involve guessing pixels."
                    .to_string(),
                "Find a control by name and return the point to click.".to_string(),
            ),
            SideEffect::Pure,
            serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        ),
    };
    ActionSpec {
        name: name.into(),
        description,
        args_schema,
        side_effect,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

impl PointerTool {
    fn new(act: Act, shared: Shared, profile: SchemaProfile) -> Self {
        Self {
            spec: spec_of(act, profile),
            act,
            shared,
        }
    }
}

fn loc_of(args: &serde_json::Value) -> Result<Loc, ToolError> {
    let n = |k: &str| args.get(k).and_then(serde_json::Value::as_f64);
    let (Some(x), Some(y)) = (n("x"), n("y")) else {
        return Err(ToolError::Failed {
            kind: "args".into(),
            detail: "needs numeric x and y".into(),
        });
    };
    Ok(
        match args.get("screen").and_then(serde_json::Value::as_str) {
            Some(s) => Loc::normalized(s, x, y),
            None => Loc::absolute(x.round() as i32, y.round() as i32),
        },
    )
}

fn button_of(args: &serde_json::Value) -> Result<Button, ToolError> {
    match args
        .get("button")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("left")
    {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        other => Err(ToolError::Failed {
            kind: "args".into(),
            detail: format!("unknown button: {other}"),
        }),
    }
}

fn failed(kind: &str, e: impl std::fmt::Display) -> ToolError {
    ToolError::Failed {
        kind: kind.into(),
        detail: e.to_string(),
    }
}

fn ok(summary: String) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput {
        summary,
        artifact: None,
        // What the remote machine reports, not what the user said.
        trust: Trust::System,
    })
}

#[async_trait]
impl Tool for PointerTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    /// What the user is being asked to approve. A confirmation prompt that
    /// says only "'pointer_click' is irreversible" tells them nothing they
    /// could decide on, so the staged description carries the coordinates and
    /// the text.
    async fn stage(&self, args: &serde_json::Value, _ctx: &ToolCtx) -> Option<StagedEffect> {
        // `{}` on a serde `Value` renders a string with its JSON quotes, and
        // a confirmation prompt is read by a person.
        let where_ = || {
            let screen = args
                .get("screen")
                .and_then(serde_json::Value::as_str)
                .map(|s| format!(" of screen {s}"))
                .unwrap_or_default();
            match (args.get("x"), args.get("y")) {
                (Some(x), Some(y)) => format!("({x}, {y}){screen}"),
                _ => "where the pointer is".to_string(),
            }
        };
        let description = match self.act {
            Act::Click => {
                let b = args
                    .get("button")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("left");
                let n = args
                    .get("count")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1);
                let times = if n > 1 {
                    format!(" {n} times")
                } else {
                    String::new()
                };
                format!("{b}-click{times} at {} on the remote machine", where_())
            }
            Act::Type => match (
                args.get("text").and_then(serde_json::Value::as_str),
                args.get("key").and_then(serde_json::Value::as_str),
            ) {
                (Some(t), _) => format!("type {t:?} on the remote machine"),
                (None, Some(k)) => {
                    let mods: Vec<String> = args
                        .get("modifiers")
                        .and_then(serde_json::Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|m| m.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    if mods.is_empty() {
                        format!("press {k} on the remote machine")
                    } else {
                        format!("press {}+{k} on the remote machine", mods.join("+"))
                    }
                }
                _ => "send input to the remote machine".to_string(),
            },
            _ => return None,
        };
        Some(StagedEffect { description })
    }

    async fn call(
        &self,
        args: &serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        match self.act {
            Act::Screens => {
                let mut s = self.shared.lock().await;
                s.refresh().await.map_err(|e| failed("pointer", e))?;
                let sc = s.screens();
                let lines: Vec<String> = sc
                    .screens
                    .iter()
                    .map(|x| {
                        format!(
                            "{} {}x{} at ({},{}) scale {}{}",
                            x.id.0,
                            x.bounds.w,
                            x.bounds.h,
                            x.bounds.x,
                            x.bounds.y,
                            x.scale,
                            if x.primary { " primary" } else { "" }
                        )
                    })
                    .collect();
                ok(lines.join("; "))
            }
            Act::Position => {
                let s = self.shared.lock().await;
                let p = s.position().await.map_err(|e| failed("pointer", e))?;
                ok(format!("pointer is at ({}, {})", p.x, p.y))
            }
            Act::Move => {
                let loc = loc_of(args)?;
                let s = self.shared.lock().await;
                let p = s.resolve(&loc).map_err(|e| failed("args", e))?;
                s.move_to(&loc).await.map_err(|e| failed("pointer", e))?;
                ok(format!("moved to ({}, {})", p.x, p.y))
            }
            Act::Click => {
                let button = button_of(args)?;
                let count = args
                    .get("count")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1) as u32;
                let s = self.shared.lock().await;
                match loc_of(args) {
                    Ok(loc) => {
                        let p = s.resolve(&loc).map_err(|e| failed("args", e))?;
                        s.click_at(&loc, button, count)
                            .await
                            .map_err(|e| failed("pointer", e))?;
                        ok(format!("clicked ({}, {})", p.x, p.y))
                    }
                    // No coordinates: click where the pointer already is.
                    Err(_) => {
                        s.click_here(button, count)
                            .await
                            .map_err(|e| failed("pointer", e))?;
                        ok("clicked where the pointer was".into())
                    }
                }
            }
            Act::Scroll => {
                let dx = args
                    .get("dx")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let Some(dy) = args.get("dy").and_then(serde_json::Value::as_i64) else {
                    return Err(failed("args", "needs dy"));
                };
                let s = self.shared.lock().await;
                s.scroll(dx as i32, dy as i32)
                    .await
                    .map_err(|e| failed("pointer", e))?;
                ok(format!("scrolled {dx},{dy}"))
            }
            Act::UiRead => {
                let q = args.get("query").and_then(serde_json::Value::as_str);
                let s = self.shared.lock().await;
                let v = s.ui_read(q).await.map_err(|e| failed("pointer", e))?;
                ok(v.render().trim_end().replace('\n', " | "))
            }
            Act::UiFind => {
                let Some(name) = args.get("name").and_then(serde_json::Value::as_str) else {
                    return Err(failed("args", "needs name"));
                };
                let s = self.shared.lock().await;
                let v = s
                    .ui_read(Some(name))
                    .await
                    .map_err(|e| failed("pointer", e))?;
                let hits = v.find(name);
                if hits.is_empty() {
                    return ok(format!("no control matching {name}"));
                }
                ok(hits
                    .iter()
                    .take(5)
                    .map(|n| {
                        format!(
                            "{} \"{}\" at ({}, {})",
                            n.role, n.name, n.center.x, n.center.y
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; "))
            }
            Act::ClipRead => {
                let s = self.shared.lock().await;
                let text = s.clipboard_read().await.map_err(|e| failed("pointer", e))?;
                ok(text)
            }
            Act::ClipWrite => {
                let Some(text) = args.get("text").and_then(serde_json::Value::as_str) else {
                    return Err(failed("args", "needs text"));
                };
                let s = self.shared.lock().await;
                s.clipboard_write(text)
                    .await
                    .map_err(|e| failed("pointer", e))?;
                ok(format!(
                    "put {} characters on the clipboard",
                    text.chars().count()
                ))
            }
            Act::Type => {
                let s = self.shared.lock().await;
                if let Some(text) = args.get("text").and_then(serde_json::Value::as_str) {
                    s.type_text(text).await.map_err(|e| failed("pointer", e))?;
                    return ok(format!("typed {} characters", text.chars().count()));
                }
                let Some(k) = args.get("key").and_then(serde_json::Value::as_str) else {
                    return Err(failed("args", "needs text or key"));
                };
                let key =
                    parse_key(k).ok_or_else(|| failed("args", format!("unknown key: {k}")))?;
                let mut mods = Vec::new();
                for m in args
                    .get("modifiers")
                    .and_then(serde_json::Value::as_array)
                    .unwrap_or(&vec![])
                {
                    let mk = m
                        .as_str()
                        .and_then(parse_key)
                        .ok_or_else(|| failed("args", format!("unknown modifier: {m}")))?;
                    if !mk.is_modifier() {
                        return Err(failed("args", format!("{m} is not a modifier")));
                    }
                    mods.push(mk);
                }
                if mods.is_empty() {
                    s.press(key).await.map_err(|e| failed("pointer", e))?;
                    ok(format!("pressed {k}"))
                } else {
                    s.chord(&mods, key)
                        .await
                        .map_err(|e| failed("pointer", e))?;
                    let names: Vec<String> = args
                        .get("modifiers")
                        .and_then(serde_json::Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|m| m.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    ok(format!("pressed {}+{k}", names.join("+")))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nspointer::mock::MockPointer;
    use nspointer::{Point, Rect, Screen, ScreenId, Screens};

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
                label: "main".into(),
            }],
            state: 2,
        }
    }

    async fn built() -> (Vec<Arc<dyn Tool>>, Arc<MockPointer>) {
        let mock = Arc::new(MockPointer::new(layout(), Point::new(0, 0)));
        let t = tools(mock.clone() as Arc<dyn Pointer>, SchemaProfile::Full)
            .await
            .unwrap();
        (t, mock)
    }

    fn find<'a>(t: &'a [Arc<dyn Tool>], name: &str) -> &'a Arc<dyn Tool> {
        t.iter().find(|x| x.spec().name == name).expect(name)
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            session: nscore::SessionId("t".into()),
            artifacts: None,
        }
    }

    /// The classification is the point of this adapter: a click on someone
    /// else's desktop must reach `SideEffectGate`, and a read must not.
    #[tokio::test]
    async fn reads_are_pure_and_input_that_activates_something_is_irreversible() {
        let (t, _) = built().await;
        let effect = |n: &str| find(&t, n).spec().side_effect;
        assert_eq!(effect("pointer_screens"), SideEffect::Pure);
        assert_eq!(effect("pointer_position"), SideEffect::Pure);
        assert_eq!(effect("pointer_move"), SideEffect::Reversible);
        assert_eq!(effect("pointer_scroll"), SideEffect::Reversible);
        assert_eq!(effect("pointer_click"), SideEffect::Irreversible);
        assert_eq!(effect("pointer_type"), SideEffect::Irreversible);
    }

    /// "Confirm 'pointer_click'" is not a question anyone can answer.
    #[tokio::test]
    async fn the_confirmation_prompt_says_what_will_actually_happen() {
        let (t, _) = built().await;
        async fn staged(
            t: &[Arc<dyn Tool>],
            n: &str,
            a: serde_json::Value,
        ) -> Option<StagedEffect> {
            find(t, n).stage(&a, &ctx()).await
        }
        assert_eq!(
            staged(&t, "pointer_click", serde_json::json!({"x": 800, "y": 400}))
                .await
                .unwrap()
                .description,
            "left-click at (800, 400) on the remote machine"
        );
        assert_eq!(
            staged(
                &t,
                "pointer_click",
                serde_json::json!({"x": 0.5, "y": 0.5, "screen": "S1", "button": "right", "count": 2})
            )
            .await
            .unwrap()
            .description,
            "right-click 2 times at (0.5, 0.5) of screen S1 on the remote machine"
        );
        assert_eq!(
            staged(&t, "pointer_type", serde_json::json!({"text": "rm -rf /"}))
                .await
                .unwrap()
                .description,
            "type \"rm -rf /\" on the remote machine"
        );
        assert_eq!(
            staged(
                &t,
                "pointer_type",
                serde_json::json!({"key": "c", "modifiers": ["ctrl"]})
            )
            .await
            .unwrap()
            .description,
            "press ctrl+c on the remote machine"
        );
        // Reversible actions are not staged: the gate only asks about the
        // ones there is no undo for.
        assert!(
            staged(&t, "pointer_move", serde_json::json!({"x": 1, "y": 1}))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_click_reaches_the_pointer_with_the_right_coordinates() {
        let (t, mock) = built().await;
        let out = find(&t, "pointer_click")
            .call(
                &serde_json::json!({"x": 0.5, "y": 0.5, "screen": "S1"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out.summary, "clicked (960, 540)");
        assert_eq!(out.trust, Trust::System);
        assert_eq!(*mock.track().last().unwrap(), Point::new(960, 540));
    }

    #[tokio::test]
    async fn screens_are_rendered_for_a_reply_model_not_as_json() {
        let (t, _) = built().await;
        let out = find(&t, "pointer_screens")
            .call(&serde_json::json!({}), &ctx())
            .await
            .unwrap();
        assert_eq!(out.summary, "S1 1920x1080 at (0,0) scale 1 primary");
        // Engine syntax stays out of model-visible text (the entrainment
        // plan's phase-1 invariant applies to every tool, not just recall).
        assert!(!out.summary.contains(" = "));
        assert!(!out.summary.starts_with('['));
    }

    #[tokio::test]
    async fn bad_arguments_are_tool_errors_not_silent_guesses() {
        let (t, mock) = built().await;
        for (name, args) in [
            ("pointer_move", serde_json::json!({"x": 5})),
            (
                "pointer_move",
                serde_json::json!({"x": 5, "y": 5, "screen": "NOPE"}),
            ),
            ("pointer_scroll", serde_json::json!({})),
            ("pointer_type", serde_json::json!({})),
            ("pointer_type", serde_json::json!({"key": "wat"})),
            (
                "pointer_type",
                serde_json::json!({"key": "a", "modifiers": ["enter"]}),
            ),
        ] {
            assert!(
                find(&t, name).call(&args, &ctx()).await.is_err(),
                "{name} {args} should fail"
            );
        }
        assert!(mock.steps().is_empty(), "nothing reached the machine");
    }

    #[tokio::test]
    async fn typing_and_chords_go_through_as_the_right_steps() {
        let (t, mock) = built().await;
        let ty = find(&t, "pointer_type");
        assert_eq!(
            ty.call(&serde_json::json!({"text": "ok@x"}), &ctx())
                .await
                .unwrap()
                .summary,
            "typed 4 characters"
        );
        assert_eq!(
            ty.call(
                &serde_json::json!({"key": "c", "modifiers": ["ctrl"]}),
                &ctx()
            )
            .await
            .unwrap()
            .summary,
            "pressed ctrl+c"
        );
        let texts: Vec<String> = mock
            .steps()
            .iter()
            .filter_map(|s| match s {
                nspointer::Step::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.concat(), "ok@x");
    }

    /// All six actions drive one connection, so they cannot disagree about
    /// the layout they are resolving against.
    /// Reading the clipboard is a read; replacing it is not irreversible in
    /// the way a click is — nothing gets activated.
    #[tokio::test]
    async fn the_clipboard_actions_carry_the_right_effect_and_round_trip() {
        let (t, _) = built().await;
        assert_eq!(
            find(&t, "pointer_clipboard_read").spec().side_effect,
            SideEffect::Pure
        );
        assert_eq!(
            find(&t, "pointer_clipboard_write").spec().side_effect,
            SideEffect::Reversible
        );
        assert!(find(&t, "pointer_clipboard_write")
            .stage(&serde_json::json!({"text": "x"}), &ctx())
            .await
            .is_none());

        let out = find(&t, "pointer_clipboard_write")
            .call(
                &serde_json::json!({"text": "four thousand characters, notionally"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out.summary, "put 36 characters on the clipboard");
        assert_eq!(
            find(&t, "pointer_clipboard_read")
                .call(&serde_json::json!({}), &ctx())
                .await
                .unwrap()
                .summary,
            "four thousand characters, notionally"
        );
    }

    #[tokio::test]
    async fn every_action_shares_one_session() {
        let (t, mock) = built().await;
        assert_eq!(t.len(), 10);
        find(&t, "pointer_move")
            .call(&serde_json::json!({"x": 100, "y": 100}), &ctx())
            .await
            .unwrap();
        let out = find(&t, "pointer_position")
            .call(&serde_json::json!({}), &ctx())
            .await
            .unwrap();
        assert_eq!(out.summary, "pointer is at (100, 100)");
        assert!(!mock.steps().is_empty());
    }

    fn spec_named(profile: SchemaProfile, name: &str) -> ActionSpec {
        specs(profile)
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} is missing from the {} set", profile.as_str()))
    }

    /// M10 T1.2. The convention used to be written three times per tool (once
    /// per axis, `screen` included) and twice over, in `pointer_click` and
    /// `pointer_move` — ~280 duplicated chars, and those two were the
    /// recorded array's top carriers at 218 and 174 tokens. It is one fact
    /// about the tool, so it is stated once in the tool's description, and
    /// the two tools state the *same* one: an emitter that learns the
    /// convention from `pointer_move` applies it to `pointer_click`.
    #[test]
    fn click_and_move_share_one_coordinate_convention_under_two_hundred_chars() {
        for profile in [SchemaProfile::Full, SchemaProfile::Slim] {
            let convention = coords(profile);
            assert!(
                convention.chars().count() <= 200,
                "the {} convention is {} chars",
                profile.as_str(),
                convention.chars().count()
            );
            for name in ["pointer_click", "pointer_move"] {
                let spec = spec_named(profile, name);
                assert!(
                    spec.description.contains(convention),
                    "{name} ({}) does not carry the shared convention: {:?}",
                    profile.as_str(),
                    spec.description
                );
                // One statement of it, and only in the description: the axes
                // carry a distinguishing phrase, not the sentence again.
                let props = spec.args_schema["properties"].as_object().unwrap();
                for axis in ["x", "y", "screen"] {
                    let d = props[axis]["description"].as_str().unwrap();
                    assert!(
                        d.chars().count() <= 30,
                        "{name}.{axis} restates the convention: {d:?}"
                    );
                }
            }
        }
    }

    /// M10 T1.3. Every action name and every required parameter survives the
    /// slim profile — the saving is text, never capability.
    #[test]
    fn the_slim_profile_keeps_every_pointer_action_and_required_parameter() {
        let full = specs(SchemaProfile::Full);
        let slim = specs(SchemaProfile::Slim);
        assert_eq!(full.len(), slim.len());
        for (f, s) in full.iter().zip(slim.iter()) {
            assert_eq!(f.name, s.name, "the slim set reorders or renames actions");
            assert_eq!(f.side_effect, s.side_effect, "{} changed effect", f.name);
            assert_eq!(
                f.args_schema.get("required"),
                s.args_schema.get("required"),
                "{} lost or gained a required parameter",
                f.name
            );
            let fp = f.args_schema["properties"].as_object().unwrap();
            let sp = s.args_schema["properties"].as_object().unwrap();
            assert_eq!(
                fp.keys().collect::<Vec<_>>(),
                sp.keys().collect::<Vec<_>>(),
                "{} lost a parameter — a removed parameter is a removed capability",
                f.name
            );
            assert!(
                s.description.len() <= f.description.len(),
                "{} got longer under slim",
                f.name
            );
        }
        // The one enum the log never exercised, collapsed — and only that.
        let buttons = |set: &[ActionSpec]| {
            set.iter()
                .find(|s| s.name == "pointer_click")
                .unwrap()
                .args_schema["properties"]["button"]["enum"]
                .clone()
        };
        assert_eq!(
            buttons(&full),
            serde_json::json!(["left", "right", "middle"])
        );
        assert_eq!(buttons(&slim), serde_json::json!(["left", "right"]));
    }

    /// M10 T1.5. The recorded log's one `Malformed` rejection was
    /// `pointer_click: missing required arg "x"`, and its two
    /// `IllegalAction`s both name `pointer_move`. One argument example each,
    /// in both profiles, kept short enough to be free.
    #[test]
    fn the_two_tools_the_rejections_name_carry_an_argument_example() {
        for profile in [SchemaProfile::Full, SchemaProfile::Slim] {
            for name in ["pointer_click", "pointer_move"] {
                let spec = spec_named(profile, name);
                let example = spec
                    .description
                    .split("Ex: ")
                    .nth(1)
                    .unwrap_or_else(|| panic!("{name} ({}) has no example", profile.as_str()));
                assert!(example.contains("\"x\"") && example.contains("\"y\""));
                assert!(
                    nscore::estimate_tokens(example.len()) <= 40,
                    "{name}'s example costs {} tokens",
                    nscore::estimate_tokens(example.len())
                );
            }
        }
    }

    /// A stored expected string per tool per profile (M10 T1.3): any change
    /// to the text the emitter is shown shows up here as a diff to review,
    /// which is the only instrument this tier has for "a shortened
    /// description changed what the model does" short of a live session.
    #[test]
    fn the_pointer_descriptions_are_what_the_snapshot_says() {
        let render = |profile: SchemaProfile| {
            specs(profile)
                .iter()
                .map(|s| format!("{}\n  {}\n  {}\n", s.name, s.description, s.args_schema))
                .collect::<String>()
        };
        let full = render(SchemaProfile::Full);
        let slim = render(SchemaProfile::Slim);
        // Not the text itself — a hash, so the snapshot is one line and a
        // drift is unmistakable. Print both on failure so the new value is
        // in the output that failed.
        // FNV-1a rather than `DefaultHasher`, whose values the standard
        // library does not promise across releases: a snapshot that changes
        // when the toolchain does is not a snapshot.
        let h = |s: &str| {
            s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |acc, b| {
                (acc ^ b as u64).wrapping_mul(0x100_0000_01b3)
            })
        };
        assert_eq!(
            (h(&full), h(&slim)),
            (FULL_SNAPSHOT, SLIM_SNAPSHOT),
            "the desktop tool text changed.\n--- full ---\n{full}\n--- slim ---\n{slim}"
        );
    }

    const FULL_SNAPSHOT: u64 = 9396470299233653983;
    const SLIM_SNAPSHOT: u64 = 8071011069512019417;
}

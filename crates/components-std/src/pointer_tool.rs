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
use nscore::{ActionSpec, SideEffect, StagedEffect, Tool, ToolCtx, ToolError, ToolOutput, Trust};
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
}

pub struct PointerTool {
    spec: ActionSpec,
    act: Act,
    shared: Shared,
}

/// Every action over one connection. Register the lot with `HarnessBuilder`.
pub async fn tools(pointer: Arc<dyn Pointer>) -> Result<Vec<Arc<dyn Tool>>, String> {
    let session = Session::open(pointer).await.map_err(|e| e.to_string())?;
    let shared: Shared = Arc::new(Mutex::new(session));
    Ok([
        Act::Screens,
        Act::Position,
        Act::Move,
        Act::Click,
        Act::Scroll,
        Act::Type,
    ]
    .into_iter()
    .map(|act| Arc::new(PointerTool::new(act, shared.clone())) as Arc<dyn Tool>)
    .collect())
}

fn xy_schema(extra: serde_json::Value) -> serde_json::Value {
    let mut base = serde_json::json!({
        "type": "object",
        "properties": {
            "x": {"type": "number", "description":
                "Absolute pixel from the left of the virtual desktop, or 0.0-1.0 if screen is given."},
            "y": {"type": "number", "description":
                "Absolute pixel from the top of the virtual desktop, or 0.0-1.0 if screen is given."},
            "screen": {"type": "string", "description":
                "Optional screen id from pointer_screens; makes x and y fractions of it."},
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

impl PointerTool {
    fn new(act: Act, shared: Shared) -> Self {
        let (name, description, side_effect, args_schema) = match act {
            Act::Screens => (
                "pointer_screens",
                "List the remote machine's displays: id, pixel bounds, DPI scale, which is \
                 primary. Call before using absolute coordinates.",
                SideEffect::Pure,
                serde_json::json!({"type": "object", "properties": {}}),
            ),
            Act::Position => (
                "pointer_position",
                "Where the remote pointer is now.",
                SideEffect::Pure,
                serde_json::json!({"type": "object", "properties": {}}),
            ),
            Act::Move => (
                "pointer_move",
                "Move the remote pointer. Does not click.",
                SideEffect::Reversible,
                xy_schema(serde_json::json!({})),
            ),
            Act::Click => (
                "pointer_click",
                "Click on the remote machine. Irreversible: whatever is under the pointer \
                 will be activated.",
                SideEffect::Irreversible,
                xy_schema(serde_json::json!({
                    "button": {"type": "string", "enum": ["left", "right", "middle"]},
                    "count": {"type": "integer", "description": "2 to double-click."},
                })),
            ),
            Act::Scroll => (
                "pointer_scroll",
                "Scroll the remote machine in notches. Positive dy scrolls down.",
                SideEffect::Reversible,
                serde_json::json!({
                    "type": "object",
                    "properties": {"dx": {"type": "integer"}, "dy": {"type": "integer"}},
                    "required": ["dy"]
                }),
            ),
            Act::Type => (
                "pointer_type",
                "Type text, or press a key, on the remote machine. Irreversible: it goes to \
                 whatever has focus. Use `text` for characters and `key` (with optional \
                 `modifiers`) for Enter, F5 or a shortcut like ctrl+c.",
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
        };
        Self {
            spec: ActionSpec {
                name: name.into(),
                description: description.into(),
                args_schema,
                side_effect,
                residual_policy: Default::default(),
                dedupe_tag: None,
            },
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
        let t = tools(mock.clone() as Arc<dyn Pointer>).await.unwrap();
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
    #[tokio::test]
    async fn every_action_shares_one_session() {
        let (t, mock) = built().await;
        assert_eq!(t.len(), 6);
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
}

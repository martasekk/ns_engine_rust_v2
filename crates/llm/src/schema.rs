use nscore::LegalActionSet;

pub const RESPOND_DIRECTLY: &str = "respond_directly";

/// The think-then-commit field, and the leading underscore is load-bearing.
///
/// The design calls for a free-text rationale **first** in the emitted
/// schema (spec §172; findings 2026-09-01: constrained decoding can degrade
/// task accuracy, and a free-text field ahead of the constrained ones is the
/// adopted mitigation). Under strict mode it is the order of `properties`
/// that shapes generation — and `serde_json::Map` is a `BTreeMap` unless the
/// `preserve_order` feature is on, which it is not here. So properties
/// serialize alphabetically, and a field named `rationale` lands wherever
/// `r` sorts: after `button` for `pointer_click`, after `key` and
/// `modifiers` for `pointer_type`, after `address`, `id`, `path` or `query`
/// for anything else. For `echo`, whose only arg is `text`, it happened to
/// come first — which is why this went unnoticed.
///
/// `_` is 0x5F and every lowercase letter is 0x61 or above, so the underscore
/// restores the documented order for every argument name in this workspace,
/// with no global change of behaviour.
///
/// Turning on `preserve_order` would have been the other fix and is the
/// wrong one: `Engine::call_key` builds the repeat gate's identity as
/// `action + args` serialized, relying in a comment on equal objects
/// serializing identically, and `event_hash` re-serializes whole events
/// (args included) for a chain `verify_chain` recomputes. Both would change
/// silently.
pub const RATIONALE: &str = "_rationale";

fn rationale_prop() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "One sentence: why this action, grounded in the user's words"
    })
}

/// `respond_directly` takes nothing else, so its whole property set is the
/// rationale. Built by hand rather than in `json!` so the key is the const
/// everywhere and cannot drift from the one the emitter strips.
fn rationale_only() -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert(RATIONALE.to_string(), rationale_prop());
    serde_json::Value::Object(props)
}

/// Compile the legal action set into the request's `tools` array:
/// one strict function tool per action, `rationale` injected as a required
/// string property of every tool, plus the always-legal `respond_directly`
/// tool. The emitter physically cannot propose an illegal action.
pub fn build_tools(legal: &LegalActionSet) -> serde_json::Value {
    let mut tools: Vec<serde_json::Value> = legal
        .actions
        .iter()
        .map(|spec| {
            let mut schema = spec.args_schema.clone();
            let obj = schema.as_object_mut().expect("args_schema is an object");
            obj.entry("type").or_insert(serde_json::json!("object"));
            obj.entry("properties").or_insert(serde_json::json!({}));
            obj["properties"][RATIONALE] = rationale_prop();
            let mut required = vec![serde_json::json!(RATIONALE)];
            if let Some(existing) = obj.get("required").and_then(|r| r.as_array()) {
                required.extend(existing.iter().cloned());
            }
            obj.insert("required".into(), serde_json::Value::Array(required));
            obj.insert("additionalProperties".into(), serde_json::json!(false));
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": spec.name,
                    "description": spec.description,
                    "strict": true,
                    "parameters": schema,
                }
            })
        })
        .collect();

    tools.push(serde_json::json!({
        "type": "function",
        "function": {
            "name": RESPOND_DIRECTLY,
            "description": "No tool applies; reply to the user conversationally from the trace.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": rationale_only(),
                "required": [RATIONALE],
                "additionalProperties": false
            }
        }
    }));

    serde_json::Value::Array(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::{ActionSpec, LegalActionSet, SideEffect};

    fn legal() -> LegalActionSet {
        LegalActionSet {
            actions: vec![ActionSpec {
                name: "echo".into(),
                description: "echo text back".into(),
                args_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            }],
        }
    }

    #[test]
    fn one_strict_tool_per_action_plus_respond_directly() {
        let tools = build_tools(&legal());
        let arr = tools.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "echo");
        assert_eq!(arr[0]["function"]["strict"], true);
        assert_eq!(arr[1]["function"]["name"], RESPOND_DIRECTLY);
        assert_eq!(arr[1]["function"]["strict"], true);
    }

    #[test]
    fn rationale_is_injected_and_required_and_schema_is_closed() {
        let tools = build_tools(&legal());
        let echo = &tools.as_array().unwrap()[0]["function"]["parameters"];
        assert_eq!(echo["properties"][RATIONALE]["type"], "string");
        assert_eq!(
            echo["properties"]["text"]["type"], "string",
            "original args kept"
        );
        let required: Vec<&str> = echo["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            required,
            vec![RATIONALE, "text"],
            "rationale first (think-then-commit)"
        );
        assert_eq!(echo["additionalProperties"], false);
    }

    /// The regression this test exists for: `required` is a `Vec` and was
    /// ordered correctly all along, which is what the assertion above
    /// checked — but strict-mode generation follows the order of
    /// **`properties`**, and that is a `serde_json::Map`, i.e. a `BTreeMap`
    /// with `preserve_order` off. So the documented "rationale first" held
    /// only for arguments sorting after `r`.
    ///
    /// `echo` could never have caught it: its one argument is `text`, and
    /// `rationale` sorted ahead of that by luck. A pointer-shaped action is
    /// the case that mattered, and `button` is the letter that broke it.
    #[test]
    fn the_rationale_property_sorts_first_whatever_the_arguments_are_called() {
        let click = ActionSpec {
            name: "pointer_click".into(),
            description: "click a point".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "button": {"type": "string"},
                    "screen": {"type": "string"},
                    "x": {"type": "number"},
                    "y": {"type": "number"}
                },
                "required": ["x", "y"]
            }),
            side_effect: SideEffect::Irreversible,
            residual_policy: Default::default(),
            dedupe_tag: None,
        };
        let tools = build_tools(&LegalActionSet {
            actions: vec![click],
        });
        let props = tools.as_array().unwrap()[0]["function"]["parameters"]["properties"]
            .as_object()
            .expect("properties is an object");
        assert_eq!(
            props.keys().next().map(String::as_str),
            Some(RATIONALE),
            "generation follows properties order, got {:?}",
            props.keys().collect::<Vec<_>>()
        );
        assert!(
            "button" < "rationale" && RATIONALE < "button",
            "the underscore is what buys the ordering"
        );
    }

    #[test]
    fn narrowed_set_narrows_tools() {
        let tools = build_tools(&LegalActionSet { actions: vec![] });
        let arr = tools.as_array().unwrap();
        assert_eq!(arr.len(), 1, "only respond_directly remains");
        assert_eq!(arr[0]["function"]["name"], RESPOND_DIRECTLY);
        assert!(
            arr[0]["function"]["parameters"]["properties"][RATIONALE].is_object(),
            "the one tool that is always legal carries the rationale too"
        );
    }
}

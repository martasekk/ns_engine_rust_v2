use nscore::LegalActionSet;

pub const RESPOND_DIRECTLY: &str = "respond_directly";

fn rationale_prop() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "One sentence: why this action, grounded in the user's words"
    })
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
            obj["properties"]["rationale"] = rationale_prop();
            let mut required = vec![serde_json::json!("rationale")];
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
                "properties": { "rationale": rationale_prop() },
                "required": ["rationale"],
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
        assert_eq!(echo["properties"]["rationale"]["type"], "string");
        assert_eq!(echo["properties"]["text"]["type"], "string", "original args kept");
        let required: Vec<&str> =
            echo["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(required, vec!["rationale", "text"], "rationale first (think-then-commit)");
        assert_eq!(echo["additionalProperties"], false);
    }

    #[test]
    fn narrowed_set_narrows_tools() {
        let tools = build_tools(&LegalActionSet { actions: vec![] });
        let arr = tools.as_array().unwrap();
        assert_eq!(arr.len(), 1, "only respond_directly remains");
        assert_eq!(arr[0]["function"]["name"], RESPOND_DIRECTLY);
    }
}

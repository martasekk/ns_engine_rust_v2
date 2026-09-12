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

/// What the `_rationale` property says about itself, per tool.
///
/// M10 T1.1: this text was repeated verbatim in every tool of every array —
/// 106 chars each, a measured **25% of all tool tokens** on the recorded
/// turn-21 call (findings §8.1). It is one instruction, and one instruction
/// belongs in the system prompt, which is sent once; see
/// [`crate::emitter::RATIONALE_INSTRUCTION`], which carries the sentence this
/// used to repeat. What stays here is the label a reader of the schema alone
/// needs to know what the field is for, and nothing more.
pub const RATIONALE_HINT: &str = "why, in one clause";

fn rationale_prop() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": RATIONALE_HINT
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
    let mut tools: Vec<serde_json::Value> = legal.actions.iter().map(tool_schema).collect();
    tools.push(respond_directly_tool());
    serde_json::Value::Array(tools)
}

/// The same array without `respond_directly` (M13 T1.1).
///
/// For an act-or-answer call only, where plain text *is* the way to say "no
/// tool applies". Offering both is offering one choice twice, and M12's live
/// run priced the duplicate: on 4 of 12 chat turns the model called the tool
/// instead of answering, and each of those bought a replier call the text
/// would not have needed (M12 P6, number 1).
///
/// May be empty. That is not a degenerate case to guard against - a chat
/// turn whose tier carries no tool has nothing to offer, and a request with
/// no `tools` key at all is the cheapest correct form of it.
pub fn build_action_tools(legal: &LegalActionSet, with_reply: bool) -> serde_json::Value {
    serde_json::Value::Array(
        legal
            .actions
            .iter()
            .map(|s| tool_schema_with(s, with_reply))
            .collect(),
    )
}

/// The user-facing line an action may carry with it (M13 T3.2).
///
/// An argument rather than message content, because two models measured on
/// 2026-09-12 — `openai/gpt-5.6-luna` and `google/gemini-3.8-flash` — never
/// once returned `content` beside `tool_calls` on the chat-completions shape,
/// over 8 turns each where doing so was exactly what the closing instruction
/// asked for. An argument is inside the one thing these models do return.
///
/// `_` for the same reason `_rationale` has it: properties serialize
/// alphabetically here, and `_rationale` must stay first, which it does —
/// `a` sorts before `e`.
pub const REPLY: &str = "_reply";

/// An empty string, not a null and not an omission.
///
/// Strict mode requires every property to be in `required`, so "nothing to
/// say" needs a value. `["string","null"]` is the documented spelling and it
/// works for one such field; with two, OpenRouter answered every call with
/// `Invalid schema for function 'ask_clarification': ... Extra required key
/// '_say' supplied` for an array whose `properties` and `required` matched
/// exactly (measured 2026-09-12, and the sent bytes were checked). Something
/// between here and the model drops the second nullable union from
/// `properties` and leaves it in `required`. A plain string has no union to
/// drop, and the emitter already treats empty as absent.
fn reply_prop() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "Your final answer, when it does not depend on what this action \
    returns. The turn ends here. Empty string if you still need the result."
    })
}

/// The other half of the pair (M13 T4.1): a line said *during* the turn.
///
/// Two fields rather than one field and a flag, because what separates them
/// is not a property of the text, it is whether the turn is over — and a
/// model reading two named slots picks the right one more reliably than one
/// reading a boolean it has to reason about. The cost is roughly 30 tokens
/// per tool, against a whole request saved whenever either fires.
///
/// **The name is load-bearing and the reason is not ours.** Called `_say`,
/// every request came back `Invalid schema for function
/// 'ask_clarification': ... Extra required key '_say' supplied` — for an
/// array whose `properties` and `required` matched exactly, which was
/// checked by parsing the bytes actually sent. Renaming it to `_speak`, one
/// character of description otherwise unchanged, fixed it outright
/// (2026-09-12, openai/gpt-5.6-luna through OpenRouter). Something between
/// here and the model strips a property called `_say` from `properties` and
/// leaves it in `required`. If this ever needs renaming again, run one live
/// turn and read the error body — the schema this file emits is valid, so a
/// schema complaint is about the transport, not about us.
pub const SAY: &str = "_speak";

fn say_prop() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "Something to tell the user right now, before this action runs, when \
    they would otherwise wait in silence - that you are about to look something up, say. You \
    keep working afterwards and answer from the result. Empty string for nothing."
    })
}

/// One spec compiled to the one element `build_tools` would put in the
/// array. Split out of `build_tools` (M10 T0.1) so a report can price a
/// single tool with the same bytes the request paid for: an apportionment
/// that re-derived the envelope would drift from the array the moment the
/// compiler changed, which is the change M10 P1 is about to make.
pub fn tool_schema(spec: &nscore::ActionSpec) -> serde_json::Value {
    tool_schema_with(spec, false)
}

/// The same, with the optional `_reply` property (M13 T3.2). `false` is
/// [`tool_schema`] byte for byte, which is what every caller that prices a
/// tool or builds a proposing array still gets.
pub fn tool_schema_with(spec: &nscore::ActionSpec, with_reply: bool) -> serde_json::Value {
    let mut schema = spec.args_schema.clone();
    let obj = schema.as_object_mut().expect("args_schema is an object");
    obj.entry("type").or_insert(serde_json::json!("object"));
    obj.entry("properties").or_insert(serde_json::json!({}));
    obj["properties"][RATIONALE] = rationale_prop();
    let mut required = vec![serde_json::json!(RATIONALE)];
    if with_reply {
        obj["properties"][REPLY] = reply_prop();
        obj["properties"][SAY] = say_prop();
        required.push(serde_json::json!(REPLY));
        required.push(serde_json::json!(SAY));
    }
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
}

/// The tool every call carries whether or not anything is legal.
pub fn respond_directly_tool() -> serde_json::Value {
    serde_json::json!({
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
    })
}

/// What one tool costs of the request, in the same chars/4 the client
/// measures the whole array with (`client.rs`, `nscore::estimate_tokens`).
///
/// The per-tool envelope and the injected `_rationale` property are part of
/// this number, because they are part of the tool's bytes — on the recorded
/// desktop array the envelope alone is ~70 tokens per tool and the rationale
/// boilerplate is 25% of the total, and a per-tool table that hid either
/// would point P1 at the wrong text. What is *not* in it is the array's own
/// two brackets and its `n - 1` commas; summing these over a call therefore
/// lands within a token or so of the call's `tools_tokens`, and that gap is
/// the floor in `estimate_tokens` plus those separators.
pub fn schema_tokens(spec: &nscore::ActionSpec) -> u32 {
    schema_tokens_with(spec, false)
}

/// The same for an array that carries `_reply` (M13 T3.2), so the per-tool
/// table prices the bytes the request really sent rather than the bytes it
/// would have sent with the knob off.
pub fn schema_tokens_with(spec: &nscore::ActionSpec, with_reply: bool) -> u32 {
    nscore::estimate_tokens(tool_schema_with(spec, with_reply).to_string().len())
}

/// `schema_tokens` for the always-appended tool, which has no spec.
pub fn respond_directly_tokens() -> u32 {
    nscore::estimate_tokens(respond_directly_tool().to_string().len())
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

    /// `core` cannot depend on `llm`, so `ActionSpec::check_arg_names` spells
    /// the rationale key out again. That duplication is only safe if the two
    /// are pinned together — this is the pin.
    #[test]
    fn the_name_core_validates_against_is_the_name_compiled_into_the_schema() {
        assert_eq!(
            RATIONALE,
            nscore::RATIONALE_ARG,
            "the injected property and the name the assembly gate validates \
             against have drifted; think-then-commit is no longer checked"
        );
    }

    /// The ordering property `build_tools` relies on, stated as an implication
    /// rather than a spot check: any spec the assembly gate accepts compiles
    /// to a schema whose first property is the rationale.
    #[test]
    fn every_spec_the_gate_accepts_compiles_rationale_first() {
        let cases = [
            serde_json::json!({"text": {"type": "string"}}),
            serde_json::json!({"button": {"type": "string"}, "x": {"type": "number"}}),
            serde_json::json!({"a": {"type": "string"}, "zzz": {"type": "string"}}),
            serde_json::json!({}),
        ];
        for props in cases {
            let spec = ActionSpec {
                name: "probe".into(),
                description: "probe".into(),
                args_schema: serde_json::json!({"type": "object", "properties": props}),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            };
            spec.check_arg_names().expect("fixture is gate-clean");
            let tools = build_tools(&LegalActionSet {
                actions: vec![spec.clone()],
            });
            let compiled = tools.as_array().unwrap()[0]["function"]["parameters"]["properties"]
                .as_object()
                .unwrap();
            assert_eq!(
                compiled.keys().next().map(String::as_str),
                Some(RATIONALE),
                "gate-clean spec {:?} did not compile rationale-first",
                spec.args_schema
            );
        }
    }

    /// The pin `ns-app budget`'s per-tool table rests on: a tool priced
    /// alone is byte-identical to the same tool inside the array, so the
    /// table's rows are the request's own bytes and not a model of them.
    #[test]
    fn a_tool_priced_alone_is_the_same_bytes_as_in_the_array() {
        let legal = legal();
        let arr = build_tools(&legal);
        let arr = arr.as_array().unwrap();
        assert_eq!(
            tool_schema(&legal.actions[0]).to_string(),
            arr[0].to_string()
        );
        assert_eq!(respond_directly_tool().to_string(), arr[1].to_string());
        // And the sum of the parts is the whole, within the separators the
        // array adds: two brackets and one comma per gap.
        let parts: usize = arr.iter().map(|t| t.to_string().len()).sum();
        let whole = serde_json::Value::Array(arr.to_vec()).to_string().len();
        assert_eq!(whole, parts + 2 + (arr.len() - 1));
        assert_eq!(
            schema_tokens(&legal.actions[0]),
            nscore::estimate_tokens(arr[0].to_string().len())
        );
        assert_eq!(
            respond_directly_tokens(),
            nscore::estimate_tokens(arr[1].to_string().len())
        );
    }

    /// M10 T1.1. The property is not "the constant is short" but "no tool
    /// pays more than one clause for it", which is what a per-tool multiplier
    /// means: the recorded turn-21 array carried 106 chars × 7 tools of the
    /// same sentence, a quarter of every tool token on the call. Measured on
    /// the compiled tool, so a description sneaking back in via `json!`
    /// fails here and not in a review.
    #[test]
    fn the_rationale_property_costs_under_forty_chars_per_tool() {
        let legal = legal();
        for tool in [tool_schema(&legal.actions[0]), respond_directly_tool()] {
            let name = tool["function"]["name"].as_str().unwrap().to_string();
            let desc = tool["function"]["parameters"]["properties"][RATIONALE]["description"]
                .as_str()
                .expect("the rationale property describes itself");
            assert!(
                desc.chars().count() <= 40,
                "`{name}` pays {} chars of rationale boilerplate: {desc:?} — the instruction \
                 belongs in the emitter's system prompt, once",
                desc.chars().count()
            );
        }
        assert!(RATIONALE_HINT.chars().count() <= 40);
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

//! The actions the engine itself owns.
//!
//! Seven schemas compiled into the engine, beside the tools a deployment
//! registers. Each one gets its name as a `const` next to the spec that
//! describes it, so a handler and its schema are never renamed apart.

/// Every action the engine itself puts in a legal set, in one list.
///
/// The registry holds the tools a deployment wired in; these seven are
/// compiled in, and a report that prices a recorded tool array needs both or
/// it can price neither (M10 T0.1). Exposed as specs rather than as names
/// because the price is the schema, not the label. Nothing here is a
/// statement about which of them were *legal* on any given call — that is
/// what the manifest's `tool_names` records.
pub fn synthetic_specs(profile: nscore::SchemaProfile) -> Vec<nscore::ActionSpec> {
    vec![
        ask_clarification_spec(profile),
        confirm_pending_spec(profile),
        remember_fact_spec(profile),
        forget_fact_spec(profile),
        forget_all_spec(profile),
        recall_spec(profile),
        inspect_result_spec(profile),
    ]
}

/// Engine-owned synthetic action: ask the user one question (spec §5.1).
pub const ASK_CLARIFICATION: &str = "ask_clarification";

pub(super) fn ask_clarification_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: ASK_CLARIFICATION.into(),
        description: profile
            .pick(
                "Ask the user one short question to resolve missing or ungrounded \
                 information required by the next action.",
                "Ask the user one short question the next action needs answered.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: the user just confirmed the staged action.
pub const CONFIRM_PENDING: &str = "confirm_pending";

pub(super) fn confirm_pending_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: CONFIRM_PENDING.into(),
        description: profile
            .pick(
                "The user has just confirmed the pending action; execute it.",
                "Execute the pending action the user just confirmed.",
            )
            .into(),
        args_schema: serde_json::json!({"type": "object", "properties": {}}),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: store one durable fact.
pub const REMEMBER_FACT: &str = "remember_fact";

pub(super) fn remember_fact_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: REMEMBER_FACT.into(),
        description: profile
            .pick(
                "Store one durable fact about the user or task as key/value \
                 (dotted keys, e.g. user.name).",
                "Store one durable fact under a dotted key, e.g. user.name.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" },
                "value": { "type": "string" }
            },
            "required": ["key", "value"]
        }),
        side_effect: nscore::SideEffect::Reversible,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: soft-delete one fact (M6 §6.2).
pub const FORGET_FACT: &str = "forget_fact";

pub(super) fn forget_fact_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: FORGET_FACT.into(),
        description: profile
            .pick(
                "Delete one stored fact by its key (e.g. user.name). Only when the user \
                 explicitly asks to forget or remove something stored.",
                "Delete one stored fact by key, only when the user asks to forget it.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "key": { "type": "string" } },
            "required": ["key"]
        }),
        side_effect: nscore::SideEffect::Reversible,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: purge every fact in the session's scope
/// (M6 §6.2). Irreversible: staged behind the confirmation flow.
pub const FORGET_ALL: &str = "forget_all";

pub(super) fn forget_all_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: FORGET_ALL.into(),
        description: profile
            .pick(
                "Erase everything stored about the user; asks for confirmation first. Only \
                 when the user explicitly asks to reset or wipe the memory.",
                "Erase every stored fact, only when the user asks to wipe the memory; \
                 confirmation is asked first.",
            )
            .into(),
        args_schema: serde_json::json!({"type": "object", "properties": {}}),
        side_effect: nscore::SideEffect::Irreversible,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: search memory beyond the context (M6 §7).
pub const RECALL: &str = "recall";

pub(super) fn recall_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: RECALL.into(),
        description: profile
            .pick(
                "Search earlier turns of this conversation and stored facts for words the \
                 user is asking about. Use when the answer is not in the recent turns \
                 or facts shown.",
                "Search earlier turns and stored facts when the answer is not in what is \
                 shown.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned, engine-*run* action: the nearest earlier conversations
/// (M9 T5.3, M10 T3.6).
///
/// It has a spec because every call in the log has one — `classify` reads it
/// for provenance and a replay resolves the call through it — but it is
/// never put in a legal set and never offered to the emitter. The deep tier
/// runs it for the same reason it runs `recall` itself: an emitter iteration
/// spent asking for context is a request that bought no progress.
pub const EXEMPLARS: &str = "exemplars";

pub(super) fn exemplars_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: EXEMPLARS.into(),
        description: "Earlier conversations most like this one, by meaning.".into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: read more of a clipped tool result
/// (M7 T1.2).
pub const INSPECT_RESULT: &str = "inspect_result";

pub(super) fn inspect_result_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: INSPECT_RESULT.into(),
        description: profile
            .pick(
                "Read more of a tool result that was shown clipped. `id` is the handle in \
                 the trace, like r42. With `query`, returns the part of the result around \
                 the first match; without one, the next part. For a desktop, prefer \
                 pointer_ui_find, which searches the live screen instead.",
                "Read more of a clipped tool result by its trace handle, like r42; with a \
                 query, the part around the first match.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "query": { "type": "string" }
            },
            "required": ["id"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Does the engine answer this action itself, rather than handing it to a
/// registered tool?
///
/// The list is `run_builtin`'s, and the two must agree: that function is the
/// one place that knows which actions the engine answers, and this is the one
/// place that asks the question without dispatching.
///
/// It exists for the repeat gate. Repeating an engine-owned call really does
/// yield nothing new — the store does not move between two `recall`s inside
/// one turn — so dropping the action from the rest of the turn costs the model
/// nothing it could have used. A registered tool is the opposite: it reads or
/// moves a world that changes underneath it, and two identical calls can have
/// entirely different results. Clicking (1200, 300) before a page loads and
/// after it loads is the same call and not the same act.
pub(super) fn is_engine_owned(action: &str) -> bool {
    matches!(
        action,
        ASK_CLARIFICATION | REMEMBER_FACT | RECALL | INSPECT_RESULT | FORGET_FACT | FORGET_ALL
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builtin specs never pass through `HarnessBuilder::add_tool`, so
    /// the assembly gate does not see them. They need the same check, or the
    /// half of the legal set the engine owns itself is the unchecked half.
    /// `is_engine_owned` decides whether a repeated call costs the model the
    /// action for the rest of the turn, so it has to name exactly the actions
    /// `run_builtin` answers. A builtin added there and forgotten here would
    /// be refused forever instead of withdrawn; a registered tool wrongly
    /// listed here would be taken away mid-task, which is the failure this
    /// whole change is about.
    #[test]
    fn engine_owned_is_exactly_the_builtins() {
        for name in [
            ASK_CLARIFICATION,
            REMEMBER_FACT,
            RECALL,
            INSPECT_RESULT,
            FORGET_FACT,
            FORGET_ALL,
        ] {
            assert!(is_engine_owned(name), "{name} is answered by the engine");
        }
        for name in [
            "pointer_click",
            "pointer_type",
            "pointer_ui_find",
            "pointer_clipboard_write",
            "get_time",
            "echo",
        ] {
            assert!(
                !is_engine_owned(name),
                "{name} is a registered tool and must keep its place in the schema"
            );
        }
    }

    #[test]
    fn every_builtin_spec_keeps_the_rationale_first() {
        for spec in [nscore::SchemaProfile::Full, nscore::SchemaProfile::Slim]
            .into_iter()
            .flat_map(synthetic_specs)
        {
            let name = spec.name.clone();
            spec.check_arg_names()
                .unwrap_or_else(|e| panic!("builtin `{name}` breaks think-then-commit: {e}"));
        }
    }
}

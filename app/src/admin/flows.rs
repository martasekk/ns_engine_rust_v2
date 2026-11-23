//! The operator's Flows page: list, read, check and save flows. This file holds the
//! save-time check now; the routes arrive in chunk 7. A flow holds no code and no URLs,
//! so the worst a bad save does is narrow one company's tools.
#![allow(dead_code)] // shortcut: the routes arrive in chunk 7; remove then

use nscore::flow::{FlowDef, Guard};

/// What a save would refuse (errors) and what it would only mention (warnings).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Report {
    pub(super) errors: Vec<String>,
    pub(super) warnings: Vec<String>,
}

/// Every top-level key `FlowDef` deserializes; anything else is silently
/// dropped by serde and would be lost on the next save — which is exactly why
/// it is worth a warning.
const KNOWN_KEYS: &[&str] = &[
    "id",
    "version",
    "roles",
    "required_roles",
    "actions",
    "fast_path",
    "reply_template",
    "templates",
    "follow_up_ms",
];

pub(super) fn check_flow(body: &serde_json::Value) -> Report {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if let Some(map) = body.as_object() {
        for key in map.keys() {
            if !KNOWN_KEYS.contains(&key.as_str()) {
                warnings.push(format!(
                    "unknown field `{key}` will be dropped on save — the engine does not read it"
                ));
            }
        }
    }

    let flow: FlowDef = match serde_json::from_value(body.clone()) {
        Ok(f) => f,
        Err(e) => {
            errors.push(format!("not a valid flow document: {e}"));
            return Report { errors, warnings };
        }
    };

    for e in flow.validate() {
        errors.push(e.to_string());
    }

    // Template ids are never checked by the engine until reply time, where a
    // missing one fails the turn — catch it at save time instead. The field
    // carries a small grammar: "" is silent, "$out:action.field" takes the
    // reply from an action's output, anything else names a template.
    if let Some(t) = &flow.reply_template {
        if let Some(rest) = t.strip_prefix("$out:") {
            match rest.split_once('.') {
                Some((action, field)) if !action.is_empty() && !field.is_empty() => {
                    if !flow.actions.iter().any(|a| a.name.as_str() == action) {
                        errors.push(format!(
                            "reply_template reads `$out:{action}.{field}`, but no action is named `{action}`"
                        ));
                    }
                }
                _ => errors.push(format!(
                    "reply_template `{t}` is a malformed $out ref — the shape is $out:action.field"
                )),
            }
        } else if !t.is_empty() && !flow.templates.contains_key(t) {
            errors.push(format!(
                "reply_template names `{t}`, which is not in templates"
            ));
        }
    }
    for (i, rule) in flow.fast_path.iter().enumerate() {
        let t = &rule.reply_template;
        if t.starts_with("$out:") {
            // The fast path runs no actions, so there is no output to read.
            errors.push(format!(
                "fast_path[{i}] uses `{t}`: the fast path runs zero actions, so an action-output \
                 reply is unreachable there — name a template or use \"\" for silence"
            ));
        } else if !t.is_empty() && !flow.templates.contains_key(t) {
            errors.push(format!(
                "fast_path[{i}] names template `{t}`, which is not in templates"
            ));
        }
    }

    // Advisory only: what v2 keeps but does not act on yet.
    for action in &flow.actions {
        if let Guard::IntentIs(i) = &action.when {
            warnings.push(format!(
                "action `{}` waits for intent `{i}`: v2 has no intent classifier yet, so it is never offered",
                action.name
            ));
        }
    }
    if !flow.fast_path.is_empty() {
        warnings.push(
            "fast_path is kept but v2 does not run it yet: these rules answer nothing".into(),
        );
    }
    if flow
        .reply_template
        .as_deref()
        .is_some_and(|t| !t.is_empty())
        || !flow.templates.is_empty()
    {
        warnings.push(
            "reply_template and templates are kept but v2 does not render them yet: the model writes every reply"
                .into(),
        );
    }
    if flow.follow_up_ms.is_some() {
        warnings.push("follow_up_ms is kept but v2 does not wake conversations yet".into());
    }
    for action in &flow.actions {
        if !action.reads.is_empty() || !action.writes.is_empty() || !action.args.is_empty() {
            warnings.push(format!(
                "action `{}`: reads, writes and args are checked but v2 does not run them yet; the model fills the tool's arguments",
                action.name
            ));
        }
    }
    for action in &flow.actions {
        warnings.push(format!(
            "action `{}` offers tool `{}`: the company must have a tool by that name (its own, a module's, or a built-in one), or nothing is offered",
            action.name, action.component
        ));
    }

    Report { errors, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HELLO: &str = include_str!("../../../crates/core/tests/fixtures/flows/hello.json");
    const TRIP: &str = include_str!("../../../crates/core/tests/fixtures/flows/trip_test.json");

    #[test]
    fn a_minimal_flow_is_clean() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":null});
        let report = check_flow(&body);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn the_v1_fixtures_have_no_errors() {
        let hello: serde_json::Value = serde_json::from_str(HELLO).unwrap();
        let report = check_flow(&hello);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.contains(
            &"fast_path is kept but v2 does not run it yet: these rules answer nothing".to_string()
        ));
        assert!(report.warnings.contains(
            &"reply_template and templates are kept but v2 does not render them yet: the model writes every reply".to_string()
        ));

        let trip: serde_json::Value = serde_json::from_str(TRIP).unwrap();
        let report = check_flow(&trip);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    #[test]
    fn an_unknown_key_warns() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":null,"colour":1});
        let report = check_flow(&body);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("unknown field `colour`")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn not_a_flow_is_one_error() {
        let report = check_flow(&json!({"id": 3}));
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(report.errors[0].starts_with("not a valid flow document"));
    }

    #[test]
    fn validate_errors_are_reported() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":["ghost"],"actions":[],"fast_path":[],"reply_template":null});
        let report = check_flow(&body);
        assert_eq!(
            report.errors,
            vec!["flow does not define role ghost".to_string()]
        );
    }

    #[test]
    fn reply_template_must_name_a_template() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":"nope"});
        let report = check_flow(&body);
        assert!(
            report.errors.iter().any(|e| e.contains("not in templates")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn reply_template_out_ref_needs_its_action() {
        let missing = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":"$out:fetch.text"});
        let report = check_flow(&missing);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("no action is named `fetch`")),
            "{:?}",
            report.errors
        );

        let malformed = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":"$out:bad"});
        let report = check_flow(&malformed);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("malformed $out ref")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn fast_path_cannot_reply_from_an_action() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"reply_template":null,
            "fast_path":[{"match_on":null,"pattern":"hi","sets":{},"reply_template":"$out:a.b"}]});
        let report = check_flow(&body);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.starts_with("fast_path[0] uses")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn intent_guard_warns() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"fast_path":[],"reply_template":null,
            "actions":[{"name":"buy","component":"shop","reads":[],"writes":[],"write_scope":"conversation","args":{},
                "when":{"when":"intent_is","value":"purchase"}}]});
        let report = check_flow(&body);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.contains(
            &"action `buy` waits for intent `purchase`: v2 has no intent classifier yet, so it is never offered".to_string()
        ));
        assert!(report.warnings.contains(
            &"action `buy` offers tool `shop`: the company must have a tool by that name (its own, a module's, or a built-in one), or nothing is offered".to_string()
        ));
        assert!(!report
            .warnings
            .iter()
            .any(|w| w.contains("reads, writes and args are checked")));
    }
}

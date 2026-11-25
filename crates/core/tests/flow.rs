use nscore::flow::{ActionDecl, FlowDef, FlowError, Guard, WriteScope};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn v1_flow_files_round_trip() {
    for src in [
        include_str!("fixtures/flows/hello.json"),
        include_str!("fixtures/flows/trip_test.json"),
    ] {
        let original: serde_json::Value = serde_json::from_str(src).expect("fixture is JSON");
        let flow: FlowDef = serde_json::from_value(original.clone()).expect("fixture is a FlowDef");
        let back = serde_json::to_value(&flow).expect("FlowDef serializes");
        assert_eq!(original, back);
    }
}

#[test]
fn guard_json_shape() {
    assert_eq!(
        serde_json::to_value(Guard::RolesFilled(vec!["a".into()])).unwrap(),
        json!({"when":"roles_filled","value":["a"]})
    );
    assert_eq!(
        serde_json::to_value(Guard::Always).unwrap(),
        json!({"when":"always"})
    );
}

#[test]
fn missing_templates_and_follow_up_default() {
    let flow: FlowDef = serde_json::from_value(json!({
        "id": "f",
        "version": 1,
        "roles": {},
        "required_roles": [],
        "actions": [],
        "fast_path": [],
        "reply_template": null
    }))
    .expect("optional fields may be absent");
    assert!(flow.templates.is_empty());
    assert_eq!(flow.follow_up_ms, None);
}

#[test]
fn missing_required_field_is_an_error() {
    let result = serde_json::from_value::<FlowDef>(json!({
        "id": "f",
        "version": 1,
        "roles": {},
        "required_roles": [],
        "fast_path": [],
        "reply_template": null
    }));
    assert!(result.is_err());
}

#[test]
fn an_action_round_trips() {
    let action = ActionDecl {
        name: "act".into(),
        component: "tool".into(),
        reads: vec![],
        writes: vec![],
        write_scope: WriteScope::Conversation,
        args: BTreeMap::from([("k".to_string(), "$out:x.y".to_string())]),
        when: Guard::RoleMissing("x".into()),
    };
    let v = serde_json::to_value(&action).unwrap();
    assert_eq!(v["write_scope"], "conversation");
    assert_eq!(v["when"], json!({"when":"role_missing","value":"x"}));
    let back: ActionDecl = serde_json::from_value(v).unwrap();
    assert_eq!(back, action);
}

// ---- behaviour -------------------------------------------------------------

fn flow(v: Value) -> FlowDef {
    serde_json::from_value(v).expect("test flow is a FlowDef")
}

/// A flow with the given roles, required roles and actions (JSON values).
fn flow_with(roles: Value, required: Value, actions: Vec<Value>) -> FlowDef {
    flow(json!({
        "id": "f",
        "version": 1,
        "roles": roles,
        "required_roles": required,
        "actions": actions,
        "fast_path": [],
        "reply_template": null
    }))
}

/// One action as JSON. `args` is a JSON object of arg name to ref string.
fn act(
    name: &str,
    component: &str,
    when: Value,
    reads: &[&str],
    writes: &[&str],
    args: Value,
) -> Value {
    json!({
        "name": name,
        "component": component,
        "reads": reads,
        "writes": writes,
        "write_scope": "turn",
        "args": args,
        "when": when
    })
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn trip_test() -> FlowDef {
    serde_json::from_str(include_str!("fixtures/flows/trip_test.json")).expect("fixture")
}

#[test]
fn trip_test_fixture_validates_clean() {
    assert!(trip_test().validate().is_empty());
    let hello: FlowDef =
        serde_json::from_str(include_str!("fixtures/flows/hello.json")).expect("fixture");
    assert!(hello.validate().is_empty());
}

#[test]
fn offer_lists_missing_required_roles_in_order() {
    let offer = trip_test().offer(&set(&["trip_date"]));
    assert_eq!(offer.missing, vec!["destination", "travelers"]);
    assert!(offer.tools.is_empty());
}

#[test]
fn note_names_each_missing_role_and_its_key() {
    let flow = trip_test();
    let offer = flow.offer(&set(&["trip_date"]));
    assert_eq!(
        flow.note(&offer).as_deref(),
        Some("Flow trip_test: still needed: destination (key trip_destination), travelers (key trip_travelers). Ask the user for one at a time; when they give one, store it with remember_fact under the key shown.")
    );
    let all = set(&["trip_destination", "trip_date", "trip_travelers"]);
    assert_eq!(flow.note(&flow.offer(&all)), None);
}

#[test]
fn offer_names_components_whose_guard_holds() {
    let flow = flow_with(
        json!({"city": "city"}),
        json!([]),
        vec![
            act(
                "a",
                "weather",
                json!({"when":"roles_filled","value":["city"]}),
                &[],
                &[],
                json!({}),
            ),
            act(
                "b",
                "ask_city_tool",
                json!({"when":"role_missing","value":"city"}),
                &[],
                &[],
                json!({}),
            ),
            act(
                "c",
                "never_tool",
                json!({"when":"never"}),
                &[],
                &[],
                json!({}),
            ),
            act(
                "d",
                "intent_tool",
                json!({"when":"intent_is","value":"buy"}),
                &[],
                &[],
                json!({}),
            ),
        ],
    );
    assert_eq!(
        flow.offer(&set(&[])).tools,
        BTreeSet::from(["ask_city_tool".to_string()])
    );
    assert_eq!(
        flow.offer(&set(&["city"])).tools,
        BTreeSet::from(["weather".to_string()])
    );
}

#[test]
fn reserved_slot_prefix_is_an_error() {
    let flow = flow_with(json!({"city": "$city"}), json!([]), vec![]);
    assert_eq!(
        flow.validate(),
        vec![FlowError::ReservedSlotPrefix {
            role: "city".into(),
            slot: "$city".into()
        }]
    );
}

#[test]
fn a_slot_remember_fact_cannot_write_is_an_error() {
    for slot in ["trip date", "", ".x", "x-"] {
        let flow = flow_with(json!({"x": slot}), json!([]), vec![]);
        assert_eq!(
            flow.validate(),
            vec![FlowError::UnwritableSlot {
                role: "x".into(),
                slot: slot.into()
            }],
            "slot {slot:?}"
        );
    }
    for slot in ["user.city", "trip_date", "_x", "a-b.c"] {
        let flow = flow_with(json!({"x": slot}), json!([]), vec![]);
        assert!(flow.validate().is_empty(), "slot {slot:?}");
    }
    let flow = flow_with(json!({"x": "$x"}), json!([]), vec![]);
    assert_eq!(
        flow.validate(),
        vec![FlowError::ReservedSlotPrefix {
            role: "x".into(),
            slot: "$x".into()
        }]
    );
}

#[test]
fn guard_naming_an_undefined_role_is_an_error() {
    let flow = flow_with(
        json!({}),
        json!([]),
        vec![act(
            "a",
            "t",
            json!({"when":"roles_filled","value":["ghost"]}),
            &[],
            &[],
            json!({}),
        )],
    );
    assert_eq!(
        flow.validate(),
        vec![FlowError::UnknownRole("ghost".into())]
    );
}

#[test]
fn required_role_undefined_is_an_error() {
    let flow = flow_with(json!({}), json!(["ghost"]), vec![]);
    assert_eq!(
        flow.validate(),
        vec![FlowError::UnknownRole("ghost".into())]
    );
}

#[test]
fn out_ref_without_a_dot_is_bad_ref() {
    let flow = flow_with(
        json!({}),
        json!([]),
        vec![act(
            "a",
            "t",
            json!({"when":"always"}),
            &["$out:x"],
            &[],
            json!({}),
        )],
    );
    assert_eq!(
        flow.validate(),
        vec![FlowError::BadRef {
            decl: "a".into(),
            raw: "$out:x".into()
        }]
    );
}

#[test]
fn out_ref_with_empty_action_is_bad_ref() {
    let flow = flow_with(
        json!({}),
        json!([]),
        vec![act(
            "a",
            "t",
            json!({"when":"always"}),
            &["$out:.f"],
            &[],
            json!({}),
        )],
    );
    assert_eq!(
        flow.validate(),
        vec![FlowError::BadRef {
            decl: "a".into(),
            raw: "$out:.f".into()
        }]
    );
}

#[test]
fn read_naming_an_undefined_role_is_an_error() {
    let flow = flow_with(
        json!({}),
        json!([]),
        vec![act(
            "a",
            "t",
            json!({"when":"always"}),
            &["ghost"],
            &[],
            json!({}),
        )],
    );
    assert_eq!(
        flow.validate(),
        vec![FlowError::UnknownRole("ghost".into())]
    );
}

#[test]
fn write_naming_an_undefined_role_is_an_error() {
    let flow = flow_with(
        json!({}),
        json!([]),
        vec![act(
            "a",
            "t",
            json!({"when":"always"}),
            &[],
            &["ghost"],
            json!({}),
        )],
    );
    assert_eq!(
        flow.validate(),
        vec![FlowError::UnknownRole("ghost".into())]
    );
}

#[test]
fn arg_whose_value_is_not_a_read_is_an_error() {
    let flow = flow_with(
        json!({"city": "city"}),
        json!([]),
        vec![act(
            "a",
            "t",
            json!({"when":"always"}),
            &[],
            &[],
            json!({"k": "city"}),
        )],
    );
    assert_eq!(
        flow.validate(),
        vec![FlowError::ArgNotDeclaredAsRead {
            decl: "a".into(),
            arg: "k".into()
        }]
    );
}

#[test]
fn two_always_writers_of_one_role_conflict() {
    let flow = flow_with(
        json!({"r": "s"}),
        json!([]),
        vec![
            act("a", "t", json!({"when":"always"}), &[], &["r"], json!({})),
            act("b", "t", json!({"when":"always"}), &[], &["r"], json!({})),
        ],
    );
    assert_eq!(
        flow.validate(),
        vec![FlowError::ConflictingWriters {
            role: "r".into(),
            first: "a".into(),
            second: "b".into()
        }]
    );
}

#[test]
fn mutually_exclusive_writers_are_allowed() {
    let by_intent = flow_with(
        json!({"r": "r"}),
        json!([]),
        vec![
            act(
                "buy",
                "t",
                json!({"when":"intent_is","value":"buy"}),
                &[],
                &["r"],
                json!({}),
            ),
            act(
                "sell",
                "t",
                json!({"when":"intent_is","value":"sell"}),
                &[],
                &["r"],
                json!({}),
            ),
        ],
    );
    assert!(by_intent.validate().is_empty());

    let by_fill = flow_with(
        json!({"x": "x", "r": "r"}),
        json!([]),
        vec![
            act(
                "missing",
                "t",
                json!({"when":"role_missing","value":"x"}),
                &[],
                &["r"],
                json!({}),
            ),
            act(
                "filled",
                "t",
                json!({"when":"roles_filled","value":["x"]}),
                &[],
                &["r"],
                json!({}),
            ),
        ],
    );
    assert!(by_fill.validate().is_empty());
}

#[test]
fn duplicate_errors_are_reported_once() {
    let flow = flow_with(
        json!({}),
        json!([]),
        vec![
            act(
                "a",
                "t",
                json!({"when":"roles_filled","value":["ghost"]}),
                &[],
                &[],
                json!({}),
            ),
            act(
                "b",
                "t",
                json!({"when":"role_missing","value":"ghost"}),
                &[],
                &[],
                json!({}),
            ),
        ],
    );
    assert_eq!(
        flow.validate(),
        vec![FlowError::UnknownRole("ghost".into())]
    );
}

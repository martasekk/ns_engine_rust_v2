use nscore::flow::{ActionDecl, FlowDef, Guard, WriteScope};
use serde_json::json;
use std::collections::BTreeMap;

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

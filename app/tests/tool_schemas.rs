//! The desktop tools, compiled the way a real request compiles them.
//!
//! `schema.rs` has unit tests for the builder, but the builder is only half
//! the contract: the other half is the specs it is handed. This file is the
//! sweep over the real ones, and it lives in `app` because that is the only
//! crate that depends on both `ns-llm` and `ns-components-std`.
//!
//! What it is defending against, concretely: `tool_schema_with` sends
//! `"strict": true`, and OpenAI then requires `required` to name every key in
//! `properties`. On 2026-09-15 a desktop run died on
//!
//!   Invalid schema for function 'pointer_move': 'required' is required to be
//!   supplied and to be an array including every key in properties.
//!   Missing 'screen'.
//!
//! The provider rejects on the first offender and fails the entire request,
//! so one loose spec takes the whole turn with it.

use nscore::SchemaProfile;

/// Every property of every desktop tool is named in `required`.
#[test]
fn every_desktop_tool_is_a_valid_strict_schema() {
    for profile in [SchemaProfile::Full, SchemaProfile::Slim] {
        for with_reply in [false, true] {
            for spec in nscomponents_std::pointer_tool::specs(profile) {
                let compiled = nsllm::schema::tool_schema_with(&spec, with_reply);
                let params = &compiled["function"]["parameters"];

                assert_eq!(
                    compiled["function"]["strict"],
                    serde_json::json!(true),
                    "{}: not strict, so this test's premise no longer holds",
                    spec.name
                );

                let required: Vec<&str> = params["required"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{}: required is not an array", spec.name))
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect();

                let properties = params["properties"]
                    .as_object()
                    .unwrap_or_else(|| panic!("{}: properties is not an object", spec.name));

                for key in properties.keys() {
                    assert!(
                        required.contains(&key.as_str()),
                        "{} ({profile:?}, with_reply={with_reply}): property `{key}` is \
                         missing from `required`, which strict mode rejects with \
                         invalid_function_parameters",
                        spec.name
                    );
                }

                assert_eq!(
                    params["additionalProperties"],
                    serde_json::json!(false),
                    "{}: strict mode also requires a closed schema",
                    spec.name
                );
            }
        }
    }
}

/// The five arguments that were optional before strict mode must still be
/// omittable, which strict mode spells as a `null` in the type. Named one by
/// one rather than derived, so that widening a *mandatory* argument by
/// accident fails here.
#[test]
fn the_optional_desktop_arguments_are_still_omittable() {
    let expected: &[(&str, &[&str])] = &[
        ("pointer_move", &["screen"]),
        ("pointer_click", &["button", "count", "screen"]),
        ("pointer_scroll", &["dx"]),
        ("pointer_type", &["key", "modifiers", "text"]),
        ("pointer_ui_read", &["query"]),
    ];

    let specs = nscomponents_std::pointer_tool::specs(SchemaProfile::Full);
    for (name, nullable) in expected {
        let spec = specs
            .iter()
            .find(|s| s.name == *name)
            .unwrap_or_else(|| panic!("{name} is not a desktop action any more"));
        let compiled = nsllm::schema::tool_schema_with(spec, true);
        let props = &compiled["function"]["parameters"]["properties"];
        for key in *nullable {
            let ty = &props[*key]["type"];
            let accepts_null = ty
                .as_array()
                .map(|t| t.iter().any(|v| v.as_str() == Some("null")))
                .unwrap_or(false);
            assert!(
                accepts_null,
                "{name}.{key} was optional and must still accept null, got {ty}"
            );
        }
    }
}

/// `x` and `y` are how a click says where it lands. If these ever become
/// nullable the model may omit them and the engine will click at the origin.
#[test]
fn the_coordinates_of_a_click_stay_mandatory() {
    let specs = nscomponents_std::pointer_tool::specs(SchemaProfile::Full);
    let spec = specs
        .iter()
        .find(|s| s.name == "pointer_click")
        .expect("pointer_click is a desktop action");
    let compiled = nsllm::schema::tool_schema_with(spec, true);
    let props = &compiled["function"]["parameters"]["properties"];
    for key in ["x", "y"] {
        assert_eq!(
            props[key]["type"],
            serde_json::json!("number"),
            "pointer_click.{key} must stay a plain number, not a nullable one"
        );
    }
}

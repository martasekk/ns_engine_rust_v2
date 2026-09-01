use crate::index::ValueIndex;
use nscore::{ActionSpec, Provenance, TaggedValue, Trust};

fn value_as_match_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn find_output_leaf(v: &serde_json::Value, index: &ValueIndex) -> Option<(Provenance, Trust)> {
    index.output_leaves.iter().rev().find(|l| l.value == *v).map(|l| {
        (Provenance::CopiedOutput { call: l.call, path: l.path.clone() }, l.trust)
    })
}

fn find_user_span(
    needle: &str,
    index: &ValueIndex,
    current_turn: u32,
) -> Option<(Provenance, Trust)> {
    // Grounding requires substance: pure punctuation/whitespace substring
    // matches (seen live from degenerate emitter output) prove nothing.
    if needle.is_empty() || !needle.chars().any(|c| c.is_alphanumeric()) {
        return None;
    }
    let current = index.user_texts.iter().rev().filter(|(t, _)| *t == current_turn);
    let earlier = index.user_texts.iter().rev().filter(|(t, _)| *t != current_turn);
    for (turn, text) in current.chain(earlier) {
        if let Some(start) = text.find(needle) {
            return Some((
                Provenance::UserInput {
                    turn: *turn,
                    start: start as u32,
                    end: (start + needle.len()) as u32,
                },
                Trust::User,
            ));
        }
    }
    None
}

fn find_constant(
    arg: &str,
    v: &serde_json::Value,
    spec: &ActionSpec,
) -> Option<(Provenance, Trust)> {
    spec.args_schema["properties"][arg]["enum"]
        .as_array()
        .filter(|options| options.contains(v))
        .map(|_| (Provenance::Constant, Trust::System))
}

fn classify_value(
    arg: &str,
    v: &serde_json::Value,
    spec: &ActionSpec,
    index: &ValueIndex,
    current_turn: u32,
) -> (Provenance, Trust) {
    let needle = value_as_match_string(v);
    // 1–3: direct matches in spec order (§5.4)
    if let Some(hit) = find_output_leaf(v, index) {
        return hit;
    }
    if let Some(hit) = find_user_span(&needle, index, current_turn) {
        return hit;
    }
    if let Some(hit) = find_constant(arg, v, spec) {
        return hit;
    }
    // 4: registered normalizers over sources 1–2
    for (func, normalized) in
        [("trim", needle.trim().to_string()), ("lowercase", needle.to_lowercase())]
    {
        if normalized == needle {
            continue; // normalization changed nothing; already tried
        }
        let as_value = serde_json::Value::String(normalized.clone());
        let hit = find_output_leaf(&as_value, index)
            .or_else(|| find_user_span(&normalized, index, current_turn));
        if let Some((prov, trust)) = hit {
            return (Provenance::Transform { func: func.into(), inputs: vec![prov] }, trust);
        }
    }
    // 5: matched nothing — the model made it up. Trust::System (model-internal,
    // neither user-attested nor external; decision record in the M3 plan).
    (Provenance::Residual, Trust::System)
}

/// Classify every top-level arg of a proposal against the session's history.
/// Match order per spec §5.4: tool-result paths → user spans → constants →
/// normalizers (trim, lowercase) → Residual.
pub fn classify_args(
    args: &serde_json::Value,
    spec: &ActionSpec,
    index: &ValueIndex,
    current_turn: u32,
) -> Vec<(String, TaggedValue)> {
    args.as_object()
        .map(|map| {
            map.iter()
                .map(|(k, v)| {
                    let (prov, trust) = classify_value(k, v, spec, index, current_turn);
                    (k.clone(), TaggedValue { value: v.clone(), prov, trust })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{OutputLeaf, ValueIndex};
    use nscore::*;

    fn spec_with_enum() -> ActionSpec {
        ActionSpec {
            name: "order".into(),
            description: "d".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "priority": {"type": "string", "enum": ["low", "high"]},
                    "product": {"type": "string"},
                    "order_id": {"type": "integer"}
                },
                "required": ["product"]
            }),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }

    fn index() -> ValueIndex {
        ValueIndex {
            user_texts: vec![(1, "order a widget please".into()), (2, "make it so".into())],
            output_leaves: vec![OutputLeaf {
                call: EventId(7),
                path: "$.order.id".into(),
                value: serde_json::json!(4242),
                trust: Trust::External,
            }],
        }
    }

    fn classify_one(args: serde_json::Value) -> Vec<(String, TaggedValue)> {
        classify_args(&args, &spec_with_enum(), &index(), 2)
    }

    #[test]
    fn tool_output_leaf_wins_with_its_trust() {
        let out = classify_one(serde_json::json!({"order_id": 4242}));
        let (_, tv) = &out[0];
        assert!(
            matches!(&tv.prov, Provenance::CopiedOutput { call: EventId(7), path } if path == "$.order.id")
        );
        assert_eq!(tv.trust, Trust::External);
    }

    #[test]
    fn user_span_is_found_with_offsets() {
        let out = classify_one(serde_json::json!({"product": "widget"}));
        let (_, tv) = &out[0];
        match &tv.prov {
            Provenance::UserInput { turn, start, end } => {
                assert_eq!(*turn, 1);
                assert_eq!(&"order a widget please"[*start as usize..*end as usize], "widget");
            }
            other => panic!("expected UserInput, got {other:?}"),
        }
        assert_eq!(tv.trust, Trust::User);
    }

    #[test]
    fn enum_value_is_constant_system_trust() {
        let out = classify_one(serde_json::json!({"priority": "high"}));
        let (_, tv) = &out[0];
        assert!(matches!(tv.prov, Provenance::Constant));
        assert_eq!(tv.trust, Trust::System);
    }

    #[test]
    fn lowercase_match_is_a_transform_preserving_trust() {
        let out = classify_one(serde_json::json!({"product": "Widget"}));
        let (_, tv) = &out[0];
        match &tv.prov {
            Provenance::Transform { func, inputs } => {
                assert_eq!(func, "lowercase");
                assert!(matches!(inputs[0], Provenance::UserInput { .. }));
            }
            other => panic!("expected Transform, got {other:?}"),
        }
        assert_eq!(tv.trust, Trust::User);
    }

    #[test]
    fn unmatched_value_is_residual_system_trust() {
        let out = classify_one(serde_json::json!({"product": "flux capacitor"}));
        let (_, tv) = &out[0];
        assert!(matches!(tv.prov, Provenance::Residual));
        assert_eq!(tv.trust, Trust::System);
    }

    #[test]
    fn current_turn_user_text_is_searched_first() {
        let mut idx = index();
        idx.user_texts.push((2, "widget again".into()));
        let out =
            classify_args(&serde_json::json!({"product": "widget"}), &spec_with_enum(), &idx, 2);
        let (_, tv) = &out[0];
        assert!(matches!(&tv.prov, Provenance::UserInput { turn: 2, .. }));
    }

    #[test]
    fn trivial_punctuation_spans_do_not_ground() {
        // Seen live: a degenerate emitter output of ", " substring-matched the
        // user's text and was blessed as UserInput. Grounding requires at
        // least one alphanumeric character.
        let mut idx = index();
        idx.user_texts.push((2, "well, so be it! ?".into()));
        for junk in [", ", ",", " ", "!?", "! ?"] {
            let out =
                classify_args(&serde_json::json!({"product": junk}), &spec_with_enum(), &idx, 2);
            let (_, tv) = &out[0];
            assert!(
                matches!(tv.prov, Provenance::Residual),
                "junk {junk:?} must stay Residual, got {:?}",
                tv.prov
            );
        }
        // single meaningful characters still ground
        let out = classify_args(&serde_json::json!({"product": "b"}), &spec_with_enum(), &idx, 2);
        assert!(matches!(&out[0].1.prov, Provenance::UserInput { .. }));
    }
}

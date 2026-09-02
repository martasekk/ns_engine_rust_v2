use serde_json::Value;

/// Minimal JSON-schema subset the harness enforces on tool args before
/// classification: `required`, per-property `type`, per-property `enum`.
/// Unknown extra args are allowed (the model may pass harmless extras).
pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(obj) = args.as_object() else {
        return Err(format!("args must be an object, got {args}"));
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for r in required {
            if let Some(name) = r.as_str() {
                if !obj.contains_key(name) {
                    return Err(format!("missing required arg {name:?}"));
                }
            }
        }
    }
    let props = schema.get("properties").and_then(Value::as_object);
    for (name, value) in obj {
        let Some(prop) = props.and_then(|p| p.get(name)) else {
            continue;
        };
        if let Some(ty) = prop.get("type").and_then(Value::as_str) {
            let ok = match ty {
                "string" => value.is_string(),
                "number" => value.is_number(),
                "integer" => value.is_i64() || value.is_u64(),
                "boolean" => value.is_boolean(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                _ => true,
            };
            if !ok {
                return Err(format!("arg {name:?} must be {ty}, got {value}"));
            }
        }
        if let Some(choices) = prop.get("enum").and_then(Value::as_array) {
            if !choices.contains(value) {
                let list = Value::Array(choices.clone());
                return Err(format!("arg {name:?} must be one of {list}, got {value}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "days": {"type": "integer"},
                "unit": {"type": "string", "enum": ["c", "f"]}
            },
            "required": ["city"]
        })
    }

    #[test]
    fn valid_args_pass() {
        assert_eq!(
            validate_args(&schema(), &json!({"city": "Brno", "days": 2, "unit": "c"})),
            Ok(())
        );
        assert_eq!(
            validate_args(&schema(), &json!({"city": "Brno", "extra": 1})),
            Ok(())
        );
    }

    #[test]
    fn missing_required_fails() {
        let err = validate_args(&schema(), &json!({"days": 2})).unwrap_err();
        assert_eq!(err, r#"missing required arg "city""#);
    }

    #[test]
    fn wrong_type_fails() {
        let err = validate_args(&schema(), &json!({"city": 5})).unwrap_err();
        assert_eq!(err, r#"arg "city" must be string, got 5"#);
        let err = validate_args(&schema(), &json!({"city": "x", "days": 1.5})).unwrap_err();
        assert_eq!(err, r#"arg "days" must be integer, got 1.5"#);
    }

    #[test]
    fn enum_mismatch_fails_and_names_choices() {
        let err = validate_args(&schema(), &json!({"city": "x", "unit": "Celsius"})).unwrap_err();
        assert_eq!(err, r#"arg "unit" must be one of ["c","f"], got "Celsius""#);
    }

    #[test]
    fn non_object_args_fail_and_empty_schema_accepts_anything() {
        assert!(validate_args(&schema(), &json!("nope")).is_err());
        assert_eq!(
            validate_args(
                &json!({"type": "object", "properties": {}}),
                &json!({"a": 1})
            ),
            Ok(())
        );
    }
}

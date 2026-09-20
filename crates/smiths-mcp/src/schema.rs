//! Argument validation against a tool's declared JSON Schema.
//!
//! The dispatcher runs this before a tool sees its arguments, so a
//! tool's `inputSchema` is a contract rather than documentation.
//! Coverage is the subset that matters for the shipped tool set:
//! `type`, `required`, `enum`, nested `properties`, and array
//! `items`. Numeric bounds and `additionalProperties` are left to
//! the tool.

use serde_json::{Map, Value};

/// Validate `args` against `schema`. `Err` carries a message naming
/// the offending path.
pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    validate_value(schema, args, "arguments")
}

fn validate_value(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    if let Some(ty) = schema.get("type") {
        check_type(ty, value, path)?;
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("`{path}` must be one of {allowed:?}"));
    }
    match value {
        Value::Object(obj) => validate_object(schema, obj, path),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    validate_value(item_schema, item, &format!("{path}[{i}]"))?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_object(
    schema: &Map<String, Value>,
    obj: &Map<String, Value>,
    path: &str,
) -> Result<(), String> {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !obj.contains_key(key) {
                return Err(format!("`{path}` is missing required field `{key}`"));
            }
        }
    }
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (key, sub) in props {
            if let Some(v) = obj.get(key) {
                validate_value(sub, v, &format!("{path}.{key}"))?;
            }
        }
    }
    Ok(())
}

fn check_type(ty: &Value, value: &Value, path: &str) -> Result<(), String> {
    let names: Vec<&str> = match ty {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => return Ok(()),
    };
    if names.is_empty() || names.iter().any(|n| matches_type(n, value)) {
        Ok(())
    } else {
        Err(format!(
            "`{path}` must be of type {} (got {})",
            names.join(" | "),
            type_name(value)
        ))
    }
}

fn matches_type(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        "integer" => value
            .as_f64()
            .is_some_and(|f| f.fract() == 0.0 && f.is_finite()),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // Unknown type keywords never reject: the schema author's
        // intent is unclear, so leave the decision to the tool.
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id": { "type": "string" },
                "count":   { "type": "integer" },
                "phase":   { "type": "string", "enum": ["live", "terminated"] },
                "flag":    { "type": "boolean" },
                "messages": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "role": { "type": "string", "enum": ["user", "system"] },
                            "content": { "type": "string" }
                        },
                        "required": ["role", "content"]
                    }
                },
                "value": { "description": "anything goes" }
            },
            "required": ["call_id"]
        })
    }

    #[test]
    fn accepts_conforming_arguments() {
        let args = json!({
            "call_id": "abc", "count": 3, "phase": "live", "flag": true,
            "messages": [{"role": "user", "content": "hi"}],
            "value": {"nested": [1, 2]}
        });
        validate_args(&schema(), &args).unwrap();
    }

    #[test]
    fn rejects_missing_required_field() {
        let err = validate_args(&schema(), &json!({"count": 1})).unwrap_err();
        assert!(err.contains("call_id"), "{err}");
    }

    #[test]
    fn rejects_wrong_primitive_type() {
        let err = validate_args(&schema(), &json!({"call_id": 42})).unwrap_err();
        assert!(err.contains("call_id") && err.contains("string"), "{err}");
        let err = validate_args(&schema(), &json!({"call_id": "x", "count": 1.5})).unwrap_err();
        assert!(err.contains("count"), "{err}");
        let err = validate_args(&schema(), &json!({"call_id": "x", "flag": "yes"})).unwrap_err();
        assert!(err.contains("flag"), "{err}");
    }

    #[test]
    fn integer_accepts_whole_floats() {
        validate_args(&schema(), &json!({"call_id": "x", "count": 2.0})).unwrap();
    }

    #[test]
    fn rejects_enum_violation() {
        let err =
            validate_args(&schema(), &json!({"call_id": "x", "phase": "ringing"})).unwrap_err();
        assert!(err.contains("phase"), "{err}");
    }

    #[test]
    fn validates_nested_array_items() {
        let err = validate_args(
            &schema(),
            &json!({"call_id": "x", "messages": [{"role": "robot", "content": "hi"}]}),
        )
        .unwrap_err();
        assert!(err.contains("messages[0].role"), "{err}");
        let err = validate_args(
            &schema(),
            &json!({"call_id": "x", "messages": [{"role": "user"}]}),
        )
        .unwrap_err();
        assert!(err.contains("content"), "{err}");
    }

    #[test]
    fn non_object_arguments_are_rejected_for_object_schema() {
        let err = validate_args(&schema(), &json!("nope")).unwrap_err();
        assert!(err.contains("object"), "{err}");
    }

    #[test]
    fn schema_without_type_accepts_anything() {
        validate_args(&json!({}), &json!([1, 2, 3])).unwrap();
        validate_args(&Value::Null, &json!({"x": 1})).unwrap();
    }
}

//! Control-parameter validation against a plugin's JSON-schema-ish
//! control map.
//!
//! Spec reference: `docs/architecture/05-ai-plugin-protocol.md §Controls`.
//!
//! We deliberately do **not** drag in a full JSON-Schema engine — the
//! subset plugins actually use is small (`type`, `minimum`, `maximum`,
//! `enum`), and we want strict-reject error messages carrying
//! structured diagnostics the agent can self-correct from. Full JSON
//! Schema might come later as an opt-in feature flag; the MVP keeps
//! its validator honest and tiny.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// Structured validation failure. Serializable to JSON so MCP tools
/// can put it straight into the `error.data` field.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// `controls.rate`, `controls.stability`, etc.
    pub field: String,
    /// Human-readable reason.
    pub reason: String,
    /// Caller-corrective hint — e.g. `supported: [...]` for enum,
    /// `maximum: 2.0` for range. Empty when there is nothing useful
    /// to attach.
    pub hint: Option<Value>,
}

impl ValidationError {
    /// Render as the MCP `error.data` payload. The `field` and
    /// `reason` are always present; `hint` is merged in when set.
    #[must_use]
    pub fn into_json(self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("field".into(), Value::String(self.field));
        obj.insert("reason".into(), Value::String(self.reason));
        if let Some(h) = self.hint
            && let Value::Object(hint) = h
        {
            for (k, v) in hint {
                obj.insert(k, v);
            }
        }
        Value::Object(obj)
    }
}

/// Validate `submitted` controls against the `declared` schema. Unknown
/// keys are rejected — this is the whole point.
///
/// `declared` is the `controls` map from a [`crate::CapabilityDescriptor`]
/// (i.e. `descriptor.extra.get("controls")`).
pub fn validate_controls(
    declared: &BTreeMap<String, Value>,
    submitted: &Value,
) -> Result<(), ValidationError> {
    let Some(obj) = submitted.as_object() else {
        if submitted.is_null() {
            return Ok(());
        }
        return Err(ValidationError {
            field: "controls".into(),
            reason: "must be an object".into(),
            hint: None,
        });
    };

    for (key, value) in obj {
        let Some(schema) = declared.get(key) else {
            let supported: Vec<&String> = declared.keys().collect();
            return Err(ValidationError {
                field: format!("controls.{key}"),
                reason: "not supported by provider".into(),
                hint: Some(json!({ "supported": supported })),
            });
        };
        check_one(&format!("controls.{key}"), schema, value)?;
    }
    Ok(())
}

/// Validate one value against its declared schema entry.
fn check_one(field: &str, schema: &Value, value: &Value) -> Result<(), ValidationError> {
    let expected_type = schema.get("type").and_then(Value::as_str);
    if let Some(t) = expected_type
        && !type_matches(t, value)
    {
        return Err(ValidationError {
            field: field.to_owned(),
            reason: format!("expected type `{t}`, got {}", json_type_name(value)),
            hint: None,
        });
    }

    match expected_type {
        Some("number" | "integer") => check_range(field, schema, value)?,
        Some("string") => check_enum(field, schema, value)?,
        _ => {}
    }
    Ok(())
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn check_range(field: &str, schema: &Value, value: &Value) -> Result<(), ValidationError> {
    let Some(n) = value.as_f64() else {
        return Ok(());
    };
    if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
        && n < min
    {
        return Err(ValidationError {
            field: field.to_owned(),
            reason: "below minimum".into(),
            hint: Some(json!({ "minimum": min, "got": n })),
        });
    }
    if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
        && n > max
    {
        return Err(ValidationError {
            field: field.to_owned(),
            reason: "above maximum".into(),
            hint: Some(json!({ "maximum": max, "got": n })),
        });
    }
    Ok(())
}

fn check_enum(field: &str, schema: &Value, value: &Value) -> Result<(), ValidationError> {
    let Some(list) = schema.get("enum").and_then(Value::as_array) else {
        return Ok(());
    };
    if !list.iter().any(|v| v == value) {
        return Err(ValidationError {
            field: field.to_owned(),
            reason: "not in allowed enum".into(),
            hint: Some(json!({ "allowed": list })),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared() -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert(
            "rate".into(),
            json!({"type": "number", "minimum": 0.5, "maximum": 2.0, "default": 1.0}),
        );
        m.insert(
            "voice".into(),
            json!({"type": "string", "enum": ["a", "b"]}),
        );
        m.insert(
            "beam".into(),
            json!({"type": "integer", "minimum": 1, "maximum": 10}),
        );
        m
    }

    #[test]
    fn null_submitted_is_ok() {
        validate_controls(&declared(), &Value::Null).unwrap();
    }

    #[test]
    fn empty_object_is_ok() {
        validate_controls(&declared(), &json!({})).unwrap();
    }

    #[test]
    fn valid_values_pass() {
        validate_controls(&declared(), &json!({"rate": 1.1, "voice": "a", "beam": 3})).unwrap();
    }

    #[test]
    fn unknown_key_rejected_with_supported_list() {
        let err = validate_controls(&declared(), &json!({"stability": 0.7})).unwrap_err();
        assert_eq!(err.field, "controls.stability");
        assert_eq!(err.reason, "not supported by provider");
        let hint = err.hint.unwrap();
        let supported: Vec<String> = serde_json::from_value(hint["supported"].clone()).unwrap();
        assert_eq!(supported, vec!["beam", "rate", "voice"]);
    }

    #[test]
    fn above_maximum_rejected() {
        let err = validate_controls(&declared(), &json!({"rate": 3.0})).unwrap_err();
        assert_eq!(err.field, "controls.rate");
        assert_eq!(err.reason, "above maximum");
    }

    #[test]
    fn below_minimum_rejected() {
        let err = validate_controls(&declared(), &json!({"beam": 0})).unwrap_err();
        assert_eq!(err.field, "controls.beam");
        assert_eq!(err.reason, "below minimum");
    }

    #[test]
    fn wrong_type_rejected() {
        let err = validate_controls(&declared(), &json!({"rate": "fast"})).unwrap_err();
        assert_eq!(err.field, "controls.rate");
        assert!(err.reason.starts_with("expected type"));
    }

    #[test]
    fn enum_miss_rejected() {
        let err = validate_controls(&declared(), &json!({"voice": "c"})).unwrap_err();
        assert_eq!(err.field, "controls.voice");
        assert_eq!(err.reason, "not in allowed enum");
    }
}

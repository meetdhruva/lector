//! A minimal JSON-Schema subset for command arguments.
//!
//! Supported: `type` (`object`, `string`, `integer`, `number`, `boolean`, `array`,
//! `null`, or a list of them), `properties`, `required`, `additionalProperties: false`,
//! `items`, `enum`, `minimum` / `maximum`. Everything else is ignored (accepted), so
//! schemas can carry `description`s and `default`s for MCP clients.

use serde_json::{json, Map, Value};

/// Validates `value` against `schema`; the error names the offending path.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "args")
}

fn type_matches(ty: &str, v: &Value) -> bool {
    match ty {
        "object" => v.is_object(),
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn check(schema: &Value, v: &Value, at: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    match schema.get("type") {
        Some(Value::String(ty)) if !type_matches(ty, v) => {
            return Err(format!("{at}: expected {ty}, got {}", type_name(v)));
        }
        Some(Value::Array(tys))
            if !tys
                .iter()
                .filter_map(Value::as_str)
                .any(|t| type_matches(t, v)) =>
        {
            return Err(format!("{at}: unexpected {}", type_name(v)));
        }
        _ => {}
    }
    if let Some(Value::Array(options)) = schema.get("enum") {
        if !options.contains(v) {
            return Err(format!(
                "{at}: must be one of {}",
                Value::Array(options.clone())
            ));
        }
    }
    if let Some(n) = v.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
            if n < min {
                return Err(format!("{at}: must be >= {min}"));
            }
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
            if n > max {
                return Err(format!("{at}: must be <= {max}"));
            }
        }
    }
    if let Value::Object(obj) = v {
        let props = schema.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(required)) = schema.get("required") {
            for r in required.iter().filter_map(Value::as_str) {
                if !obj.contains_key(r) {
                    return Err(format!("{at}: missing required `{r}`"));
                }
            }
        }
        let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
        for (k, child) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(s) => check(s, child, &format!("{at}.{k}"))?,
                None if closed => return Err(format!("{at}: unknown property `{k}`")),
                None => {}
            }
        }
    }
    if let (Value::Array(items), Some(item_schema)) = (v, schema.get("items")) {
        for (i, item) in items.iter().enumerate() {
            check(item_schema, item, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

/// Builds an `object` schema: `props` are `(name, schema)` pairs, `required` names.
/// Unknown properties are rejected (agents get a clear error for typos).
pub fn object(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut map = Map::new();
    for (k, v) in props {
        map.insert((*k).to_string(), v.clone());
    }
    json!({
        "type": "object",
        "properties": map,
        "required": required,
        "additionalProperties": false,
    })
}

/// `{"type": ty, "description": desc}`.
pub fn prop(ty: &str, desc: &str) -> Value {
    json!({ "type": ty, "description": desc })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_subset() {
        let s = object(
            &[
                ("path", prop("string", "file")),
                ("line", json!({"type": "integer", "minimum": 0})),
                ("mode", json!({"type": "string", "enum": ["a", "b"]})),
                (
                    "tags",
                    json!({"type": "array", "items": {"type": "string"}}),
                ),
            ],
            &["path"],
        );
        assert!(validate(&s, &json!({"path": "x"})).is_ok());
        assert!(validate(
            &s,
            &json!({"path": "x", "line": 3, "mode": "a", "tags": ["t"]})
        )
        .is_ok());
        assert!(validate(&s, &json!({}))
            .unwrap_err()
            .contains("missing required `path`"));
        assert!(validate(&s, &json!({"path": 1}))
            .unwrap_err()
            .contains("args.path"));
        assert!(validate(&s, &json!({"path": "x", "line": -1})).is_err());
        assert!(validate(&s, &json!({"path": "x", "line": 1.5})).is_err());
        assert!(validate(&s, &json!({"path": "x", "mode": "c"})).is_err());
        assert!(validate(&s, &json!({"path": "x", "tags": [1]}))
            .unwrap_err()
            .contains("tags[0]"));
        assert!(validate(&s, &json!({"path": "x", "typo": 1}))
            .unwrap_err()
            .contains("typo"));
        assert!(validate(&s, &json!([])).is_err());
        assert!(validate(&json!({"type": ["string", "null"]}), &Value::Null).is_ok());
    }
}

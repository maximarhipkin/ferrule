//! The JSON Schema subset plugin tools declare their arguments in, and the
//! validator the host runs before a call. In-house on purpose: a full
//! validator brings a regex engine and `$ref` resolution, and a keyword the
//! host doesn't check must not look checked, so anything outside the subset
//! is refused when the plugin is installed.

use serde_json::Value;

/// Keywords the validator enforces.
const CHECKED: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
];

/// Keywords that only describe.
const ANNOTATIONS: &[&str] = &["description", "title", "default", "examples"];

const TYPES: &[&str] = &[
    "string", "number", "integer", "boolean", "object", "array", "null",
];

/// Refuse a schema that uses anything outside the subset, naming it.
pub fn check_supported(schema: &Value) -> Result<(), String> {
    walk(schema, "")
}

fn walk(schema: &Value, at: &str) -> Result<(), String> {
    let here = || {
        if at.is_empty() {
            "/".to_string()
        } else {
            at.to_string()
        }
    };
    let Some(obj) = schema.as_object() else {
        return Err(format!("schema at `{}` must be an object", here()));
    };
    for (k, v) in obj {
        if ANNOTATIONS.contains(&k.as_str()) {
            continue;
        }
        if !CHECKED.contains(&k.as_str()) {
            return Err(format!(
                "schema keyword `{k}` at `{}` isn't supported (supported: {})",
                here(),
                CHECKED.join(", ")
            ));
        }
        match k.as_str() {
            "type" => {
                let names: Vec<&Value> = match v {
                    Value::Array(a) => a.iter().collect(),
                    v => vec![v],
                };
                for n in names {
                    if !n.as_str().is_some_and(|n| TYPES.contains(&n)) {
                        return Err(format!(
                            "`type` at `{}` must be one of {}",
                            here(),
                            TYPES.join(", ")
                        ));
                    }
                }
            }
            "properties" => {
                let Some(props) = v.as_object() else {
                    return Err(format!("`properties` at `{}` must be an object", here()));
                };
                for (name, s) in props {
                    walk(s, &format!("{at}/{name}"))?;
                }
            }
            "required" => {
                if !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
                    return Err(format!(
                        "`required` at `{}` must be a list of names",
                        here()
                    ));
                }
            }
            "additionalProperties" => {
                if !v.is_boolean() {
                    walk(v, &format!("{at}/*"))?;
                }
            }
            "items" => walk(v, &format!("{at}/[]"))?,
            "enum" => {
                if !v.as_array().is_some_and(|a| !a.is_empty()) {
                    return Err(format!("`enum` at `{}` must be a non-empty list", here()));
                }
            }
            "const" => {}
            "minimum" | "maximum" => {
                if !v.is_number() {
                    return Err(format!("`{k}` at `{}` must be a number", here()));
                }
            }
            _ => {
                if !v.is_u64() {
                    return Err(format!(
                        "`{k}` at `{}` must be a non-negative integer",
                        here()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Check `value` against `schema` (already `check_supported`). The first
/// failure, as `` `/path`: what's wrong ``.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "")
}

fn check(schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    let fail = |msg: String| Err(format!("`{}`: {msg}", if at.is_empty() { "/" } else { at }));
    let Some(s) = schema.as_object() else {
        return Ok(());
    };
    if let Some(t) = s.get("type") {
        let allowed: Vec<&str> = match t {
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            t => t.as_str().into_iter().collect(),
        };
        if !allowed.iter().any(|t| is_type(value, t)) {
            return fail(format!(
                "must be {}, not {}",
                allowed.join(" or "),
                type_of(value)
            ));
        }
    }
    if let Some(options) = s.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            let list: Vec<String> = options.iter().map(Value::to_string).collect();
            return fail(format!("must be one of {}", list.join(", ")));
        }
    }
    if let Some(c) = s.get("const") {
        if c != value {
            return fail(format!("must be {c}"));
        }
    }
    if let Some(n) = value.as_f64() {
        if let Some(min) = s.get("minimum").and_then(Value::as_f64) {
            if n < min {
                return fail(format!("must be at least {min}"));
            }
        }
        if let Some(max) = s.get("maximum").and_then(Value::as_f64) {
            if n > max {
                return fail(format!("must be at most {max}"));
            }
        }
    }
    if let Some(text) = value.as_str() {
        let len = text.chars().count() as u64;
        if let Some(min) = s.get("minLength").and_then(Value::as_u64) {
            if len < min {
                return fail(format!("must be at least {min} characters"));
            }
        }
        if let Some(max) = s.get("maxLength").and_then(Value::as_u64) {
            if len > max {
                return fail(format!("must be at most {max} characters"));
            }
        }
    }
    if let Some(items) = value.as_array() {
        let len = items.len() as u64;
        if let Some(min) = s.get("minItems").and_then(Value::as_u64) {
            if len < min {
                return fail(format!("must have at least {min} items"));
            }
        }
        if let Some(max) = s.get("maxItems").and_then(Value::as_u64) {
            if len > max {
                return fail(format!("must have at most {max} items"));
            }
        }
        if let Some(item) = s.get("items") {
            for (i, v) in items.iter().enumerate() {
                check(item, v, &format!("{at}/{i}"))?;
            }
        }
    }
    if let Some(obj) = value.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        for name in s
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !obj.contains_key(name) {
                return fail(format!("`{name}` is required"));
            }
        }
        for (k, v) in obj {
            let path = format!("{at}/{k}");
            match props.and_then(|p| p.get(k)) {
                Some(sub) => check(sub, v, &path)?,
                None => match s.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        return fail(format!("`{k}` isn't an allowed field"));
                    }
                    Some(extra @ Value::Object(_)) => check(extra, v, &path)?,
                    _ => {}
                },
            }
        }
    }
    Ok(())
}

fn is_type(v: &Value, t: &str) -> bool {
    match t {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => false,
    }
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
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
                "value": {"type": "number", "minimum": 0},
                "unit": {"type": "string", "enum": ["km", "mi"]},
                "tags": {"type": "array", "items": {"type": "string", "maxLength": 3}, "maxItems": 2},
                "note": {"type": ["string", "null"], "description": "free text"}
            },
            "required": ["value", "unit"],
            "additionalProperties": false
        })
    }

    #[test]
    fn the_subset_is_accepted_and_the_rest_named() {
        check_supported(&schema()).unwrap();
        for (bad, word) in [
            (json!({"type": "object", "oneOf": []}), "oneOf"),
            (
                json!({"type": "object", "properties": {"a": {"$ref": "#/x"}}}),
                "$ref",
            ),
            (
                json!({"type": "object", "properties": {"a": {"type": "string", "format": "uri"}}}),
                "format",
            ),
            (json!({"type": "thing"}), "type"),
        ] {
            let err = check_supported(&bad).unwrap_err();
            assert!(err.contains(word), "{err}");
        }
    }

    #[test]
    fn arguments_are_checked_with_a_path() {
        let s = schema();
        validate(
            &s,
            &json!({"value": 3, "unit": "km", "tags": ["a"], "note": null}),
        )
        .unwrap();
        let cases = [
            (json!({"unit": "km"}), "`/`: `value` is required"),
            (
                json!({"value": -1, "unit": "km"}),
                "`/value`: must be at least 0",
            ),
            (
                json!({"value": 1, "unit": "ly"}),
                "`/unit`: must be one of \"km\", \"mi\"",
            ),
            (
                json!({"value": "1", "unit": "km"}),
                "`/value`: must be number, not a string",
            ),
            (
                json!({"value": 1, "unit": "km", "x": 1}),
                "`/`: `x` isn't an allowed field",
            ),
            (
                json!({"value": 1, "unit": "km", "tags": ["long"]}),
                "`/tags/0`: must be at most 3 characters",
            ),
            (
                json!({"value": 1, "unit": "km", "tags": ["a", "b", "c"]}),
                "`/tags`: must have at most 2 items",
            ),
            (json!([1]), "`/`: must be object, not an array"),
        ];
        for (args, want) in cases {
            assert_eq!(validate(&s, &args).unwrap_err(), want, "{args}");
        }
    }

    #[test]
    fn integer_accepts_whole_floats_only() {
        let s = json!({"type": "integer"});
        validate(&s, &json!(2)).unwrap();
        validate(&s, &json!(2.0)).unwrap();
        assert!(validate(&s, &json!(2.5)).is_err());
    }
}

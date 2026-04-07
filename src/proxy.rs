/// RPC proxy — forwards allowed RPC calls to the PIVX node with optional jq filtering.
///
/// PivxNodeController exposes `GET /mainnet/:method?params=a,b,c&filter=<jq>`.
/// MPW uses this for governance queries (getbudgetinfo, listmasternodes, etc).
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::api::AppState;

#[derive(Deserialize, Default)]
pub struct ProxyQuery {
    /// Comma-separated RPC parameters. Numbers and booleans are type-coerced.
    pub params: Option<String>,
    /// jq-style filter expression applied to the RPC result.
    pub filter: Option<String>,
}

/// Handle RPC proxy requests: `GET /mainnet/:method?params=...&filter=...`
pub async fn rpc_proxy(
    State(state): State<Arc<AppState>>,
    Path(method): Path<String>,
    Query(query): Query<ProxyQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Validate method against whitelist
    if !state.allowed_rpcs.iter().any(|r| r == &method) {
        return Err((StatusCode::FORBIDDEN, format!("RPC method '{method}' not allowed")));
    }

    // Parse params with type coercion (PivxNodeController compat)
    let params = parse_params(query.params.as_deref().unwrap_or(""));

    eprintln!("  [proxy] {method}({params:?})");

    // Call the node
    let result = state.rpc.proxy_call(&method, &params)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    // Apply jq filter if present
    let filtered = match &query.filter {
        Some(expr) if !expr.is_empty() => {
            apply_jq_filter(&result, expr)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("jq filter error: {e}")))?
        }
        _ => result,
    };

    Ok(Json(filtered))
}

/// Parse comma-separated params with PivxNodeController-style type coercion.
///
/// Each param is tried as: integer → float → boolean → string.
fn parse_params(raw: &str) -> Vec<Value> {
    if raw.is_empty() {
        return Vec::new();
    }

    raw.split(',')
        .map(|s| {
            let s = s.trim();
            // Try integer
            if let Ok(n) = s.parse::<i64>() {
                return Value::Number(n.into());
            }
            // Try float
            if let Ok(f) = s.parse::<f64>() {
                if let Some(n) = serde_json::Number::from_f64(f) {
                    return Value::Number(n);
                }
            }
            // Try boolean
            match s {
                "true" => return Value::Bool(true),
                "false" => return Value::Bool(false),
                _ => {}
            }
            // String
            Value::String(s.to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Minimal jq evaluator — covers the subset MPW actually uses
// ---------------------------------------------------------------------------

/// Apply a jq-style filter expression to a JSON value.
///
/// Supports the subset used by MPW:
/// - `.` — identity
/// - `.field` — object field access
/// - `.field.nested` — chained field access
/// - `.[]` — array/object iteration
/// - `expr | expr` — pipe (compose)
/// - `{a, b, c}` — object construction (shorthand)
/// - `.field?` — optional (suppress errors)
pub fn apply_jq_filter(value: &Value, expr: &str) -> Result<Value, String> {
    let expr = expr.trim();
    if expr.is_empty() || expr == "." {
        return Ok(value.clone());
    }

    // Split on top-level pipe (not inside braces)
    if let Some((left, right)) = split_pipe(expr) {
        let intermediate = apply_jq_filter(value, left)?;
        return apply_jq_filter(&intermediate, right);
    }

    // Object construction: {a, b, c} or {a: .field, b: .other}
    if expr.starts_with('{') && expr.ends_with('}') {
        return eval_object_construction(value, &expr[1..expr.len()-1]);
    }

    // Array iteration: .[]
    if expr == ".[]" {
        return eval_array_iter(value);
    }

    // Field access: .field or .field.nested or .field?
    if let Some(field_expr) = expr.strip_prefix('.') {
        let optional = field_expr.ends_with('?');
        let field_expr = if optional { field_expr.strip_suffix('?').unwrap() } else { field_expr };

        // Handle .[] after field access: .field[]
        if let Some(field) = field_expr.strip_suffix("[]") {
            let accessed = access_field(value, field, optional)?;
            return eval_array_iter(&accessed);
        }

        return access_field(value, field_expr, optional);
    }

    Err(format!("unsupported jq expression: {expr}"))
}

/// Split on top-level pipe `|` (not inside braces or brackets).
fn split_pipe(expr: &str) -> Option<(&str, &str)> {
    let mut depth = 0;
    for (i, c) in expr.char_indices() {
        match c {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            '|' if depth == 0 => {
                return Some((expr[..i].trim(), expr[i+1..].trim()));
            }
            _ => {}
        }
    }
    None
}

/// Access a (possibly nested) field on a value.
fn access_field(value: &Value, field_path: &str, optional: bool) -> Result<Value, String> {
    // Handle array — apply field access to each element
    if let Value::Array(arr) = value {
        let results: Vec<Value> = arr.iter()
            .filter_map(|item| access_field(item, field_path, true).ok())
            .filter(|v| !v.is_null())
            .collect();
        return Ok(Value::Array(results));
    }

    let mut current = value;
    for field in field_path.split('.') {
        if field.is_empty() { continue; }
        match current.get(field) {
            Some(v) => current = v,
            None => {
                if optional {
                    return Ok(Value::Null);
                }
                return Err(format!("field '{field}' not found"));
            }
        }
    }
    Ok(current.clone())
}

/// Iterate over array elements (or object values).
fn eval_array_iter(value: &Value) -> Result<Value, String> {
    match value {
        Value::Array(arr) => Ok(Value::Array(arr.clone())),
        Value::Object(obj) => Ok(Value::Array(obj.values().cloned().collect())),
        _ => Err("cannot iterate over non-array/object".into()),
    }
}

/// Evaluate object construction: `{a, b, c}` or `{a: .field, b: .other}`.
fn eval_object_construction(value: &Value, fields_str: &str) -> Result<Value, String> {
    // Handle array input — apply construction to each element
    if let Value::Array(arr) = value {
        let results: Vec<Value> = arr.iter()
            .map(|item| eval_object_construction(item, fields_str))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::Array(results));
    }

    let mut obj = serde_json::Map::new();

    for field_def in split_fields(fields_str) {
        let field_def = field_def.trim();
        if field_def.is_empty() { continue; }

        if let Some((key, val_expr)) = field_def.split_once(':') {
            // Explicit: {key: .value_expr}
            let key = key.trim().trim_matches('"');
            let val = apply_jq_filter(value, val_expr.trim())?;
            obj.insert(key.to_string(), val);
        } else {
            // Shorthand: {fieldname} = {fieldname: .fieldname}
            let key = field_def.trim().trim_matches('"');
            let val = value.get(key).cloned().unwrap_or(Value::Null);
            obj.insert(key.to_string(), val);
        }
    }

    Ok(Value::Object(obj))
}

/// Split field definitions by comma, respecting nested expressions.
fn split_fields(s: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut depth = 0;
    let mut start = 0;

    for (i, c) in s.char_indices() {
        match c {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                fields.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    fields.push(&s[start..]);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_params_empty() {
        assert_eq!(parse_params(""), Vec::<Value>::new());
    }

    #[test]
    fn parse_params_types() {
        let params = parse_params("42,3.14,true,hello");
        assert_eq!(params[0], json!(42));
        assert_eq!(params[1], json!(3.14));
        assert_eq!(params[2], json!(true));
        assert_eq!(params[3], json!("hello"));
    }

    #[test]
    fn jq_identity() {
        let v = json!({"a": 1});
        assert_eq!(apply_jq_filter(&v, ".").unwrap(), v);
    }

    #[test]
    fn jq_field_access() {
        let v = json!({"name": "test", "value": 42});
        assert_eq!(apply_jq_filter(&v, ".name").unwrap(), json!("test"));
        assert_eq!(apply_jq_filter(&v, ".value").unwrap(), json!(42));
    }

    #[test]
    fn jq_nested_field() {
        let v = json!({"a": {"b": {"c": 99}}});
        assert_eq!(apply_jq_filter(&v, ".a.b.c").unwrap(), json!(99));
    }

    #[test]
    fn jq_array_iter() {
        let v = json!([1, 2, 3]);
        assert_eq!(apply_jq_filter(&v, ".[]").unwrap(), json!([1, 2, 3]));
    }

    #[test]
    fn jq_pipe() {
        let v = json!({"data": [1, 2, 3]});
        assert_eq!(apply_jq_filter(&v, ".data | .[]").unwrap(), json!([1, 2, 3]));
    }

    #[test]
    fn jq_object_construction_shorthand() {
        let v = json!({"name": "Alice", "age": 30, "extra": "skip"});
        let result = apply_jq_filter(&v, "{name, age}").unwrap();
        assert_eq!(result, json!({"name": "Alice", "age": 30}));
    }

    #[test]
    fn jq_array_iter_then_object() {
        let v = json!([
            {"name": "A", "Hash": "abc", "extra": 1},
            {"name": "B", "Hash": "def", "extra": 2},
        ]);
        let result = apply_jq_filter(&v, ".[] | {name, Hash}").unwrap();
        assert_eq!(result, json!([
            {"name": "A", "Hash": "abc"},
            {"name": "B", "Hash": "def"},
        ]));
    }

    #[test]
    fn jq_optional_field() {
        let v = json!({"a": 1});
        assert_eq!(apply_jq_filter(&v, ".missing?").unwrap(), Value::Null);
    }
}

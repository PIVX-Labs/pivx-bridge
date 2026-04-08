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

    // Apply jq filter if present — shells out to system `jq` for full compatibility
    let filtered = match &query.filter {
        Some(expr) if !expr.is_empty() => {
            if expr.len() > 10_000 {
                return Err((StatusCode::BAD_REQUEST, "filter expression too long".into()));
            }
            apply_jq_system(&result, expr)
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

/// Apply a jq filter by shelling out to the system `jq` binary.
///
/// This gives 100% jq compatibility — same as PivxNodeController's `node-jq`.
fn apply_jq_system(value: &Value, expr: &str) -> Result<Value, String> {
    let input = serde_json::to_string(value).map_err(|e| e.to_string())?;

    let output = std::process::Command::new("jq")
        .arg("-c")
        .arg(expr)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(input.as_bytes())?;
            child.wait_with_output()
        })
        .map_err(|e| format!("jq execution failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("jq error: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    // jq may output multiple lines (one per result) — collect as array
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() == 1 {
        serde_json::from_str(lines[0]).map_err(|e| format!("jq output parse error: {e}"))
    } else {
        let values: Result<Vec<Value>, _> = lines.iter()
            .map(|line| serde_json::from_str(line))
            .collect();
        Ok(Value::Array(values.map_err(|e| format!("jq output parse error: {e}"))?))
    }
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
        assert_eq!(apply_jq_system(&v, ".").unwrap(), v);
    }

    #[test]
    fn jq_field_access() {
        let v = json!({"name": "test", "value": 42});
        assert_eq!(apply_jq_system(&v, ".name").unwrap(), json!("test"));
        assert_eq!(apply_jq_system(&v, ".value").unwrap(), json!(42));
    }

    #[test]
    fn jq_nested_field() {
        let v = json!({"a": {"b": {"c": 99}}});
        assert_eq!(apply_jq_system(&v, ".a.b.c").unwrap(), json!(99));
    }

    #[test]
    fn jq_pipe() {
        let v = json!({"data": [1, 2, 3]});
        assert_eq!(apply_jq_system(&v, ".data | .[]").unwrap(), json!([1, 2, 3]));
    }

    #[test]
    fn jq_object_construction() {
        let v = json!({"name": "Alice", "age": 30, "extra": "skip"});
        assert_eq!(apply_jq_system(&v, "{name, age}").unwrap(), json!({"name": "Alice", "age": 30}));
    }

    #[test]
    fn jq_complex_assignment() {
        // The expression MPW actually uses
        let v = json!({"tx": [{"hex": "abc", "txid": "123", "extra": true}]});
        let result = apply_jq_system(&v, ". | .txs = [.tx[] | { hex: .hex, txid: .txid}] | del(.tx)").unwrap();
        assert_eq!(result, json!({"txs": [{"hex": "abc", "txid": "123"}]}));
    }
}

/// JSON-RPC 1.0 client with Basic auth and connection pooling.
use serde_json::{json, Value};

pub struct RpcClient {
    url: String,
    user: String,
    pass: String,
    agent: ureq::Agent,
}

impl RpcClient {
    pub fn new(url: &str, user: &str, pass: &str) -> Self {
        Self {
            url: url.to_string(),
            user: user.to_string(),
            pass: pass.to_string(),
            agent: ureq::AgentBuilder::new()
                .max_idle_connections(4)
                .timeout(std::time::Duration::from_secs(30))
                .build(),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn pass(&self) -> &str {
        &self.pass
    }

    /// Send a JSON-RPC 1.0 request and return the "result" field.
    fn call(&self, method: &str, params: &[Value]) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "1.0",
            "id": "pivx-bridge",
            "method": method,
            "params": params,
        });

        let response = self.agent
            .post(&self.url)
            .set("Authorization", &format!(
                "Basic {}",
                base64_encode(&format!("{}:{}", self.user, self.pass))
            ))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());

        match response {
            Ok(resp) => {
                let json: Value = resp.into_json()
                    .map_err(|e| format!("JSON parse error: {e}"))?;
                if let Some(err) = json.get("error").and_then(|e| {
                    if e.is_null() { None } else { Some(e) }
                }) {
                    let msg = err.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown RPC error");
                    return Err(msg.to_string());
                }
                Ok(json.get("result").cloned().unwrap_or(Value::Null))
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                // Try to extract the RPC error message from the JSON body
                if let Ok(json) = serde_json::from_str::<Value>(&body) {
                    if let Some(msg) = json.pointer("/error/message").and_then(|m| m.as_str()) {
                        return Err(msg.to_string());
                    }
                }
                Err(format!("HTTP {code}: {body}"))
            }
            Err(e) => Err(format!("connection error: {e}")),
        }
    }

    // -- Convenience methods --

    pub fn get_block_count(&self) -> Result<u64, String> {
        self.call("getblockcount", &[])?
            .as_u64()
            .ok_or_else(|| "getblockcount: not a number".into())
    }

    pub fn get_block_hash(&self, height: u32) -> Result<String, String> {
        self.call("getblockhash", &[json!(height)])?
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "getblockhash: not a string".into())
    }

    pub fn get_block(&self, hash: &str, verbosity: u32) -> Result<Value, String> {
        self.call("getblock", &[json!(hash), json!(verbosity)])
    }

    pub fn get_raw_transaction(&self, txid: &str, verbose: bool) -> Result<Value, String> {
        self.call("getrawtransaction", &[json!(txid), json!(verbose)])
    }

    pub fn send_raw_transaction(&self, hex: &str) -> Result<String, String> {
        self.call("sendrawtransaction", &[json!(hex)])?
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "sendrawtransaction: unexpected response".into())
    }

    /// Generic RPC call for the proxy endpoint.
    pub fn proxy_call(&self, method: &str, params: &[Value]) -> Result<Value, String> {
        self.call(method, params)
    }
}

/// Minimal base64 encoder (no external crate needed for this).
fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18 & 0x3F) as usize] as char);
        out.push(ALPHABET[(triple >> 12 & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

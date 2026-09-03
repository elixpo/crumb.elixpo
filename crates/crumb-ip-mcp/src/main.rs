//! IP Geolocation MCP server.
use std::io::{self, BufRead, Write};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

fn handle_call(params: &Value) -> Result<Value> {
    let name = params.get("name").and_then(|v| v.as_str());
    if name != Some("ip") { bail!("Unknown tool"); }
    
    let args = params.get("arguments").ok_or_else(|| anyhow::anyhow!("Missing arguments"))?;
    let ip = args.get("ip").and_then(|v| v.as_str()).unwrap_or("");

    let url = format!("http://ip-api.com/json/{}", ip);

    let client = reqwest::blocking::Client::builder().user_agent("crumb-plugin/1.0").build()?;
    let mut response_text = client.get(&url).send().context("Network error")?.text()?;
    if response_text.trim().is_empty() {
        response_text = "{}".to_string();
    }
    let resp: Value = serde_json::from_str(&response_text).unwrap_or(json!({}));
    
    let output = { let city = resp.get("city").and_then(|v| v.as_str()).unwrap_or("Unknown"); let country = resp.get("country").and_then(|v| v.as_str()).unwrap_or("Unknown"); format!("IP {} is located in {}, {}", ip, city, country) };
    
    Ok(json!({ "content": [{"type": "text", "text": output}], "isError": false }))
}

fn main() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut line = String::new();

    while reader.read_line(&mut line)? > 0 {
        if line.trim().is_empty() { line.clear(); continue; }
        let Ok(value) = serde_json::from_str::<Value>(&line) else { line.clear(); continue; };
        if let Some(obj) = value.as_object() {
            if let Some(method) = obj.get("method").and_then(|m| m.as_str()) {
                let id = obj.get("id").unwrap_or(&Value::Null);
                let response = match method {
                    "server/discover" => json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2026-07-28", "capabilities": {"tools": {}}, "serverInfo": {"name": "crumb-ip-mcp", "version": "1.0.0"}}}),
                    "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}, "serverInfo": {"name": "crumb-ip-mcp", "version": "1.0.0"}}}),
                    "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{
                        "name": "ip", "description": "Look up IP geolocation using ip-api.com",
                        "inputSchema": {"type": "object", "properties": {"ip": {"type": "string"}}, "required": ["ip"]},
                        "annotations": {"readOnlyHint": true, "openWorldHint": true}
                    }]}}),
                    "tools/call" => {
                        let params = obj.get("params").unwrap_or(&Value::Null);
                        match handle_call(params) {
                            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                            Err(e) => json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": e.to_string()}], "isError": true}})
                        }
                    }
                    _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
                };
                if id != &Value::Null {
                    if let Ok(mut encoded) = serde_json::to_vec(&response) {
                        encoded.push(b'\n'); let _ = stdout.write_all(&encoded); let _ = stdout.flush();
                    }
                }
            }
        }
        line.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_handle_call_missing_args() {
        let params = json!({"name": "ip", "arguments": {}});
        let _ = handle_call(&params); // Just ensure it doesn't panic
    }
}

//! Newline-delimited JSON-RPC wire types for the public Harness SDK protocol.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const JSON_RPC_VERSION: &str = "2.0";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams<'a> {
    pub cwd: &'a Path,
    pub provider: &'a str,
    pub model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams<'a> {
    pub session_id: &'a str,
    pub content_blocks: [TextContent<'a>; 1],
}

impl<'a> SessionPromptParams<'a> {
    #[must_use]
    pub const fn text(session_id: &'a str, text: &'a str) -> Self {
        Self {
            session_id,
            content_blocks: [TextContent { kind: "text", text }],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TextContent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub id: Value,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum IncomingFrame {
    Response(Response),
    Notification(Notification),
}

impl IncomingFrame {
    /// Parses one complete JSON-RPC line from Harness stdout.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid JSON-RPC versions or unsupported frame
    /// shapes. Callers may ignore malformed lines only within a bounded wait.
    pub fn parse(line: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(line).context("invalid JSON-RPC JSON")?;
        let object = value
            .as_object()
            .context("JSON-RPC frame must be an object")?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some(JSON_RPC_VERSION) {
            bail!("unsupported JSON-RPC version");
        }
        if let Some(id) = object.get("id") {
            let result = object.get("result").cloned();
            let error = object
                .get("error")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .context("invalid JSON-RPC error")?;
            if result.is_some() == error.is_some() {
                bail!("JSON-RPC response requires exactly one result or error");
            }
            return Ok(Self::Response(Response {
                id: id.clone(),
                result,
                error,
            }));
        }
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .context("JSON-RPC notification requires a method")?;
        Ok(Self::Notification(Notification {
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }))
    }
}

#[derive(Serialize)]
struct Request<P> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<P>,
}

/// Encodes the version-sensitive Harness initialization request.
///
/// # Errors
///
/// Returns an error if a parameter cannot be serialized.
pub fn encode_initialize(id: u64, params: InitializeParams<'_>) -> Result<Vec<u8>> {
    encode_request(id, "initialize", Some(params))
}

/// Encodes one durable session prompt admission request.
///
/// # Errors
///
/// Returns an error if a parameter cannot be serialized.
pub fn encode_session_prompt(id: u64, params: SessionPromptParams<'_>) -> Result<Vec<u8>> {
    encode_request(id, "session/prompt", Some(params))
}

/// Encodes a graceful process shutdown request.
///
/// # Errors
///
/// Returns an error if the request cannot be serialized.
pub fn encode_shutdown(id: u64) -> Result<Vec<u8>> {
    encode_request::<Value>(id, "shutdown", None)
}

fn encode_request<P: Serialize>(
    id: u64,
    method: &'static str,
    params: Option<P>,
) -> Result<Vec<u8>> {
    let request = Request {
        jsonrpc: JSON_RPC_VERSION,
        id,
        method,
        params,
    };
    let mut encoded = serde_json::to_vec(&request).context("failed to encode JSON-RPC request")?;
    encoded.push(b'\n');
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::{
        IncomingFrame, InitializeParams, SessionPromptParams, encode_initialize,
        encode_session_prompt, encode_shutdown,
    };

    #[test]
    fn initialize_carries_exact_model_effort() {
        let line = encode_initialize(
            1,
            InitializeParams {
                cwd: Path::new("/workspace"),
                provider: "pollinations",
                model: "qwen-coder",
                reasoning_effort: Some("high"),
                max_tokens: Some(4096),
            },
        )
        .expect("request encodes");
        let value: serde_json::Value = serde_json::from_slice(&line).expect("valid JSON");
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["reasoningEffort"], "high");
        assert_eq!(value["params"]["maxTokens"], 4096);
    }

    #[test]
    fn prompt_uses_one_text_content_block() {
        let line = encode_session_prompt(2, SessionPromptParams::text("session-1", "hello"))
            .expect("request encodes");
        let value: serde_json::Value = serde_json::from_slice(&line).expect("valid JSON");
        assert_eq!(
            value["params"]["contentBlocks"][0],
            json!({"type":"text","text":"hello"})
        );
    }

    #[test]
    fn shutdown_omits_params() {
        let line = encode_shutdown(3).expect("request encodes");
        let value: serde_json::Value = serde_json::from_slice(&line).expect("valid JSON");
        assert!(value.get("params").is_none());
    }

    #[test]
    fn incoming_notifications_and_responses_are_distinct() {
        let notification = IncomingFrame::parse(
            r#"{"jsonrpc":"2.0","method":"session.status","params":{"status":"idle"}}"#,
        )
        .expect("notification parses");
        assert!(matches!(notification, IncomingFrame::Notification(_)));

        let response = IncomingFrame::parse(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
            .expect("response parses");
        assert!(matches!(response, IncomingFrame::Response(_)));
    }

    #[test]
    fn invalid_response_shape_is_rejected() {
        assert!(
            IncomingFrame::parse(
                r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"bad"}}"#
            )
            .is_err()
        );
    }
}

use crumb_llm::{
    ChatEvent, ChatRequest, ChatRole, EmbeddingRequest, EmbeddingResponse, FinishReason,
    ModelCapability, ModelInfo, ProviderError, ProviderErrorKind, ProviderResult, TokenUsage,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub(crate) struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: Vec<ChatCompletionMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream_options: StreamOptions,
}

impl<'a> From<&'a ChatRequest> for ChatCompletionRequest<'a> {
    fn from(request: &'a ChatRequest) -> Self {
        Self {
            model: &request.model,
            messages: request
                .messages
                .iter()
                .map(|message| ChatCompletionMessage {
                    role: role_name(message.role),
                    content: &message.content,
                })
                .collect(),
            stream: true,
            max_tokens: request.max_output_tokens,
            stream_options: StreamOptions {
                include_usage: true,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

const fn role_name(role: ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

impl<'a> From<&'a EmbeddingRequest> for WireEmbeddingRequest<'a> {
    fn from(request: &'a EmbeddingRequest) -> Self {
        Self {
            model: &request.model,
            input: &request.input,
            dimensions: request.dimensions,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireEmbeddingResponse {
    data: Vec<WireEmbedding>,
    usage: WireEmbeddingUsage,
}

#[derive(Debug, Deserialize)]
struct WireEmbedding {
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Debug, Deserialize)]
struct WireEmbeddingUsage {
    prompt_tokens: u64,
}

impl From<WireEmbeddingResponse> for EmbeddingResponse {
    fn from(mut response: WireEmbeddingResponse) -> Self {
        response.data.sort_by_key(|item| item.index);
        Self {
            vectors: response
                .data
                .into_iter()
                .map(|item| item.embedding)
                .collect(),
            usage: TokenUsage {
                input_tokens: response.usage.prompt_tokens,
                output_tokens: 0,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum TextModelsResponse {
    Models(Vec<WireModel>),
    Wrapped { data: Vec<WireModel> },
}

impl TextModelsResponse {
    pub(crate) fn into_models(self) -> Vec<ModelInfo> {
        let models = match self {
            Self::Models(models) | Self::Wrapped { data: models } => models,
        };
        models.into_iter().map(WireModel::into_model).collect()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireModel {
    #[serde(alias = "name")]
    id: String,
    #[serde(default, alias = "displayName")]
    display_name: Option<String>,
    #[serde(default, alias = "contextLength")]
    context_window: Option<u64>,
    #[serde(default)]
    capabilities: serde_json::Value,
    #[serde(default, alias = "inputModalities")]
    input_modalities: Vec<String>,
}

impl WireModel {
    fn into_model(self) -> ModelInfo {
        let mut capabilities = vec![ModelCapability::Chat, ModelCapability::Streaming];
        if capability_enabled(&self.capabilities, "tool_calling") {
            capabilities.push(ModelCapability::Tools);
        }
        if self
            .input_modalities
            .iter()
            .any(|modality| modality == "image")
        {
            capabilities.push(ModelCapability::Vision);
        }
        ModelInfo {
            display_name: self.display_name.unwrap_or_else(|| self.id.clone()),
            id: self.id,
            capabilities,
            context_window: self.context_window,
        }
    }
}

fn capability_enabled(capabilities: &serde_json::Value, name: &str) -> bool {
    match capabilities {
        serde_json::Value::Array(values) => values.iter().any(|value| value == name),
        serde_json::Value::Object(values) => {
            values.get(name).and_then(serde_json::Value::as_bool) == Some(true)
        }
        _ => false,
    }
}

/// Incremental decoder for OpenAI-compatible server-sent events.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    pending: Vec<u8>,
    done: bool,
    finish_reason: Option<FinishReason>,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> ProviderResult<Vec<ChatEvent>> {
        if self.done && !bytes.is_empty() {
            return Err(protocol_error("received data after the stream finished"));
        }
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();

        while let Some((end, delimiter_length)) = find_event_boundary(&self.pending) {
            let event = self.pending.drain(..end).collect::<Vec<_>>();
            self.pending.drain(..delimiter_length);
            self.decode_event(&event, &mut events)?;
        }
        Ok(events)
    }

    pub(crate) fn finish(self) -> ProviderResult<()> {
        if self.done && self.pending.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err(protocol_error("stream ended before the SSE done marker"))
        }
    }

    fn decode_event(&mut self, event: &[u8], output: &mut Vec<ChatEvent>) -> ProviderResult<()> {
        let text = std::str::from_utf8(event)
            .map_err(|_| protocol_error("stream event is not valid UTF-8"))?;
        let data = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            output.push(ChatEvent::Finished(
                self.finish_reason
                    .take()
                    .unwrap_or_else(|| FinishReason::Other("done".to_owned())),
            ));
            self.done = true;
            return Ok(());
        }

        let chunk: ChatCompletionChunk = serde_json::from_str(&data)
            .map_err(|_| protocol_error("stream contains malformed chat JSON"))?;
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                output.push(ChatEvent::TextDelta(content));
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(map_finish_reason(&reason));
            }
        }
        if let Some(usage) = chunk.usage {
            output.push(ChatEvent::Usage(TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            }));
        }
        Ok(())
    }
}

fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|window| window == b"\n\n");
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(left), Some(right)) if left <= right => Some((left, 2)),
        (Some(_), Some(right)) | (None, Some(right)) => Some((right, 4)),
        (Some(left), None) => Some((left, 2)),
        (None, None) => None,
    }
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCall,
        other => FinishReason::Other(other.to_owned()),
    }
}

fn protocol_error(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Protocol, message, false)
}

#[cfg(test)]
mod tests {
    use crumb_llm::{
        ChatEvent, ChatMessage, ChatRequest, ChatRole, FinishReason, ProviderErrorKind, TokenUsage,
    };

    use super::{ChatCompletionRequest, SseDecoder};

    #[test]
    fn chat_request_uses_openai_roles_and_streaming() {
        let request = ChatRequest {
            model: "openai".to_owned(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_owned(),
            }],
            max_output_tokens: Some(64),
        };

        let json = serde_json::to_value(ChatCompletionRequest::from(&request))
            .expect("request should serialize");

        assert_eq!(json["model"], "openai");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "hello");
        assert_eq!(json["stream"], true);
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert_eq!(json["max_tokens"], 64);
    }

    #[test]
    fn decoder_handles_split_unicode_sse_and_usage() {
        let payload = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hé\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        );
        let split = payload.find("é").expect("unicode text should exist") + 1;
        let mut decoder = SseDecoder::default();

        let mut events = decoder
            .push(&payload.as_bytes()[..split])
            .expect("partial event should be retained");
        events.extend(
            decoder
                .push(&payload.as_bytes()[split..])
                .expect("remaining events should decode"),
        );
        decoder.finish().expect("stream should finish cleanly");

        assert_eq!(
            events,
            vec![
                ChatEvent::TextDelta("hé".to_owned()),
                ChatEvent::Usage(TokenUsage {
                    input_tokens: 3,
                    output_tokens: 1,
                }),
                ChatEvent::Finished(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn malformed_chat_payload_is_a_non_retryable_protocol_error() {
        let mut decoder = SseDecoder::default();

        let error = decoder
            .push(b"data: {not-json}\n\n")
            .expect_err("malformed JSON should fail");

        assert_eq!(error.kind, ProviderErrorKind::Protocol);
        assert!(!error.retryable);
    }
}

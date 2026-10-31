use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::types::{
    ApiToolParam, ContentBlock, ImageContentSource, Message, MessageRole,
    SystemBlock, ThinkingConfig, Usage,
};

use super::provider::{
    ApiType, LLMProvider, ProviderRequest, ProviderResponse, StreamChunk,
};
use super::ApiError;

const API_VERSION: &str = "2023-06-01";

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u64,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiToolParam>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Value,
}

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    custom_headers: HashMap<String, String>,
}

impl AnthropicProvider {
    pub fn new(
        client: Client,
        api_key: String,
        base_url: String,
        custom_headers: HashMap<String, String>,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url,
            custom_headers,
        }
    }
}

#[async_trait]
impl LLMProvider for AnthropicProvider {
    fn api_type(&self) -> ApiType {
        ApiType::AnthropicMessages
    }

    async fn create_message(
        &self,
        request: ProviderRequest<'_>,
    ) -> Result<ProviderResponse, ApiError> {
        let mut rx = self.create_message_stream(request).await?;
        let mut text_content = String::new();
        let mut thinking_content = String::new();
        let mut tool_calls: HashMap<usize, (String, String, String)> = HashMap::new();
        let mut usage = Usage::default();
        let mut stop_reason: Option<String> = None;

        while let Some(chunk) = rx.recv().await {
            match chunk? {
                StreamChunk::TextDelta(t) => text_content.push_str(&t),
                StreamChunk::ThinkingDelta(t) => thinking_content.push_str(&t),
                StreamChunk::ToolUseStart { index, id, name } => {
                    let entry = tool_calls.entry(index).or_insert_with(|| {
                        (String::new(), String::new(), String::new())
                    });
                    if !id.is_empty() {
                        entry.0 = id;
                    }
                    if !name.is_empty() {
                        entry.1 = name;
                    }
                }
                StreamChunk::ToolUseDelta {
                    index,
                    partial_json,
                } => {
                    let entry = tool_calls
                        .entry(index)
                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                    entry.2.push_str(&partial_json);
                }
                StreamChunk::Usage(u) => usage = u,
                StreamChunk::StopReason(sr) => stop_reason = Some(sr),
                StreamChunk::Done => break,
            }
        }

        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        if !thinking_content.is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                thinking: thinking_content,
                signature: None,
            });
        }
        if !text_content.is_empty() {
            content_blocks.push(ContentBlock::Text { text: text_content });
        }
        let mut sorted: Vec<(usize, (String, String, String))> =
            tool_calls.into_iter().collect();
        sorted.sort_by_key(|(idx, _)| *idx);
        for (_, (id, name, args)) in sorted {
            let input: Value =
                serde_json::from_str(&args).unwrap_or(Value::Object(serde_json::Map::new()));
            content_blocks.push(ContentBlock::ToolUse {
                id: if id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4())
                } else {
                    id
                },
                name,
                input,
            });
        }

        Ok(ProviderResponse {
            message: Message {
                role: MessageRole::Assistant,
                content: content_blocks,
            },
            usage,
            stop_reason,
        })
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest<'_>,
    ) -> Result<Receiver<Result<StreamChunk, ApiError>>, ApiError> {
        let response = self.send_request(request).await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamChunk, ApiError>>(128);
        tokio::spawn(parse_anthropic_stream_into(response, tx));
        Ok(rx)
    }
}

impl AnthropicProvider {
    async fn send_request(
        &self,
        request: ProviderRequest<'_>,
    ) -> Result<reqwest::Response, ApiError> {
        let api_messages: Vec<AnthropicMessage> = request
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                };
                AnthropicMessage {
                    role: role.to_string(),
                    content: serde_json::to_value(&m.content).unwrap_or(Value::Array(vec![])),
                }
            })
            .collect();

        let body = AnthropicRequest {
            model: request.model.to_string(),
            max_tokens: request.max_tokens,
            messages: api_messages,
            system: request.system,
            tools: if request.tools.as_ref().map_or(true, |t| t.is_empty()) {
                None
            } else {
                request.tools
            },
            stream: true,
            thinking: request.thinking,
        };

        let mut req_builder = self
            .client
            .post(format!(
                "{}/v1/messages",
                self.base_url.trim_end_matches('/').trim_end_matches("/v1")
            ))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", "prompt-caching-2024-07-31")
            .header("content-type", "application/json");

        for (key, value) in &self.custom_headers {
            req_builder = req_builder.header(key, value);
        }

        let response = req_builder.json(&body).send().await.map_err(|e| {
            if e.is_timeout() {
                ApiError::Timeout
            } else {
                ApiError::NetworkError(e.to_string())
            }
        })?;

        let status = response.status().as_u16();
        if status == 401 {
            return Err(ApiError::AuthError("Invalid API key".to_string()));
        }
        if status == 429 {
            return Err(ApiError::RateLimitError);
        }
        if status >= 400 {
            let text = response.text().await.unwrap_or_default();
            if text.contains("prompt is too long") {
                return Err(ApiError::PromptTooLong(text));
            }
            return Err(ApiError::HttpError {
                status,
                message: text,
            });
        }

        Ok(response)
    }
}

/// Parse the Anthropic SSE byte stream and push chunks into the sender.
async fn parse_anthropic_stream_into(
    response: reqwest::Response,
    tx: Sender<Result<StreamChunk, ApiError>>,
) {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut current_blocks: HashMap<usize, Value> = HashMap::new();
    let mut usage = Usage::default();

    while let Some(chunk_result) = stream.next().await {
        let bytes = match chunk_result {
            Ok(b) => b,
            Err(e) => {
                let _ = tx.send(Err(ApiError::NetworkError(e.to_string()))).await;
                return;
            }
        };
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].trim().to_string();
            buffer = buffer[pos + 1..].to_string();

            if line.is_empty() || !line.starts_with("data: ") {
                continue;
            }
            let data = &line[6..];
            if data == "[DONE]" {
                let _ = tx.send(Ok(StreamChunk::Done)).await;
                return;
            }

            let event: Value = match serde_json::from_str(data) {
                Ok(e) => e,
                Err(_) => continue,
            };

            let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match event_type {
                "message_start" => {
                    if let Some(u) = event.pointer("/message/usage") {
                        if let Ok(u) = serde_json::from_value::<Usage>(u.clone()) {
                            usage.input_tokens = u.input_tokens;
                            usage.cache_creation_input_tokens = u.cache_creation_input_tokens;
                            usage.cache_read_input_tokens = u.cache_read_input_tokens;
                        }
                    }
                }
                "content_block_start" => {
                    if let (Some(idx), Some(block)) = (
                        event.get("index").and_then(|i| i.as_u64()).map(|i| i as usize),
                        event.get("content_block"),
                    ) {
                        current_blocks.insert(idx, block.clone());
                        // Emit ToolUseStart so downstream can show the tool name early.
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            let id = block
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let _ = tx
                                .send(Ok(StreamChunk::ToolUseStart { index: idx, id, name }))
                                .await;
                        }
                    }
                }
                "content_block_delta" => {
                    let idx = event.get("index").and_then(|i| i.as_u64()).map(|i| i as usize);
                    let delta = event.get("delta");
                    if let (Some(idx), Some(delta)) = (idx, delta) {
                        let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match delta_type {
                            "text_delta" => {
                                if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                    let _ = tx
                                        .send(Ok(StreamChunk::TextDelta(text.to_string())))
                                        .await;
                                    let block = current_blocks.entry(idx).or_insert_with(|| {
                                        serde_json::json!({"type": "text", "text": ""})
                                    });
                                    let existing =
                                        block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                    block["text"] =
                                        Value::String(format!("{}{}", existing, text));
                                }
                            }
                            "input_json_delta" => {
                                if let Some(partial) =
                                    delta.get("partial_json").and_then(|t| t.as_str())
                                {
                                    let _ = tx
                                        .send(Ok(StreamChunk::ToolUseDelta {
                                            index: idx,
                                            partial_json: partial.to_string(),
                                        }))
                                        .await;
                                    let block = current_blocks.entry(idx).or_insert_with(|| {
                                        serde_json::json!({"type": "tool_use", "id": "", "name": "", "input": {}})
                                    });
                                    let existing = block
                                        .get("_partial_json")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("");
                                    block["_partial_json"] =
                                        Value::String(format!("{}{}", existing, partial));
                                }
                            }
                            "thinking_delta" => {
                                if let Some(thinking) =
                                    delta.get("thinking").and_then(|t| t.as_str())
                                {
                                    let _ = tx
                                        .send(Ok(StreamChunk::ThinkingDelta(thinking.to_string())))
                                        .await;
                                    let block = current_blocks.entry(idx).or_insert_with(|| {
                                        serde_json::json!({"type": "thinking", "thinking": ""})
                                    });
                                    let existing = block
                                        .get("thinking")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("");
                                    block["thinking"] =
                                        Value::String(format!("{}{}", existing, thinking));
                                }
                            }
                            "signature_delta" => {
                                if let Some(sig) = delta.get("signature").and_then(|t| t.as_str()) {
                                    if let Some(block) = current_blocks.get_mut(&idx) {
                                        block["signature"] = Value::String(sig.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    let idx = event.get("index").and_then(|i| i.as_u64()).map(|i| i as usize);
                    if let Some(idx) = idx {
                        current_blocks.remove(&idx);
                    }
                }
                "message_delta" => {
                    if let Some(sr) = event.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                        let _ = tx
                            .send(Ok(StreamChunk::StopReason(sr.to_string())))
                            .await;
                    }
                    if let Some(u) = event.get("usage") {
                        if let Some(out) = u.get("output_tokens").and_then(|o| o.as_u64()) {
                            usage.output_tokens = out;
                        }
                        let _ = tx.send(Ok(StreamChunk::Usage(usage.clone()))).await;
                    }
                }
                _ => {}
            }
        }
    }

    let _ = tx.send(Ok(StreamChunk::Done)).await;
}

fn parse_anthropic_content_block(mut block: Value) -> Option<ContentBlock> {
    let block_type = block.get("type")?.as_str()?.to_string();
    match block_type.as_str() {
        "text" => Some(ContentBlock::Text {
            text: block.get("text")?.as_str()?.to_string(),
        }),
        "tool_use" => {
            let id = block.get("id")?.as_str()?.to_string();
            let name = block.get("name")?.as_str()?.to_string();
            let input = if let Some(partial) = block.get("_partial_json").and_then(|p| p.as_str()) {
                serde_json::from_str(partial).unwrap_or(Value::Object(serde_json::Map::new()))
            } else {
                block.get("input").cloned().unwrap_or(Value::Object(serde_json::Map::new()))
            };
            if let Some(obj) = block.as_object_mut() {
                obj.remove("_partial_json");
            }
            Some(ContentBlock::ToolUse { id, name, input })
        }
        "thinking" => Some(ContentBlock::Thinking {
            thinking: block.get("thinking")?.as_str()?.to_string(),
            signature: block.get("signature").and_then(|s| s.as_str()).map(String::from),
        }),
        "image" => {
            let source = block.get("source")?;
            Some(ContentBlock::Image {
                source: ImageContentSource {
                    source_type: source.get("type")?.as_str()?.to_string(),
                    media_type: source.get("media_type")?.as_str()?.to_string(),
                    data: source.get("data")?.as_str()?.to_string(),
                },
            })
        }
        _ => None,
    }
}

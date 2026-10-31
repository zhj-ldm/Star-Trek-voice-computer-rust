use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::types::{ContentBlock, Message, MessageRole, Usage};

use super::provider::{
    ApiType, LLMProvider, ProviderRequest, ProviderResponse, StreamChunk,
};
use super::ApiError;

// --- OpenAI request types ---

#[derive(Debug, Serialize)]
struct OpenAIRequest {
    model: String,
    max_tokens: u64,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    stream: bool,
    /// 思考开关（Ollama 原生协议字段；OpenAI 兼容端点忽略未知字段，不会报错）
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OpenAIFunction,
}

#[derive(Debug, Serialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAIFunctionCall,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String,
}

/// OpenAI Chat Completions API provider.
pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    base_url: String,
    custom_headers: HashMap<String, String>,
}

impl OpenAIProvider {
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
impl LLMProvider for OpenAIProvider {
    fn api_type(&self) -> ApiType {
        ApiType::OpenAICompletions
    }

    async fn create_message(
        &self,
        request: ProviderRequest<'_>,
    ) -> Result<ProviderResponse, ApiError> {
        // Non-streaming path: drive the streaming implementation and assemble
        // the full response from emitted chunks.
        let mut rx = self.create_message_stream(request).await?;
        let mut text_content = String::new();
        let mut tool_calls: HashMap<usize, OpenAIToolCall> = HashMap::new();
        let mut usage = Usage::default();
        let mut stop_reason: Option<String> = None;

        while let Some(chunk) = rx.recv().await {
            match chunk? {
                StreamChunk::TextDelta(t) => text_content.push_str(&t),
                StreamChunk::ToolUseStart { index, id, name } => {
                    let entry = tool_calls.entry(index).or_insert_with(|| OpenAIToolCall {
                        id: String::new(),
                        call_type: "function".to_string(),
                        function: OpenAIFunctionCall {
                            name: String::new(),
                            arguments: String::new(),
                        },
                    });
                    entry.id = id;
                    entry.function.name = name;
                }
                StreamChunk::ToolUseDelta {
                    index,
                    partial_json,
                } => {
                    let entry = tool_calls.entry(index).or_insert_with(|| OpenAIToolCall {
                        id: String::new(),
                        call_type: "function".to_string(),
                        function: OpenAIFunctionCall {
                            name: String::new(),
                            arguments: String::new(),
                        },
                    });
                    entry.function.arguments.push_str(&partial_json);
                }
                StreamChunk::Usage(u) => usage = u,
                StreamChunk::StopReason(sr) => stop_reason = Some(sr),
                StreamChunk::ThinkingDelta(_) => {}
                StreamChunk::Done => break,
            }
        }

        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        if !text_content.is_empty() {
            content_blocks.push(ContentBlock::Text { text: text_content });
        }
        let mut sorted_calls: Vec<(usize, OpenAIToolCall)> = tool_calls.into_iter().collect();
        sorted_calls.sort_by_key(|(idx, _)| *idx);
        for (_, tc) in sorted_calls {
            let input: Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or(Value::Object(serde_json::Map::new()));
            content_blocks.push(ContentBlock::ToolUse {
                id: if tc.id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4())
                } else {
                    tc.id
                },
                name: tc.function.name,
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
        tokio::spawn(parse_openai_stream_into(response, tx));
        Ok(rx)
    }
}

impl OpenAIProvider {
    /// Build and send the HTTP request, returning the raw response on success.
    async fn send_request(
        &self,
        request: ProviderRequest<'_>,
    ) -> Result<reqwest::Response, ApiError> {
        let mut openai_messages = Vec::new();

        // System prompt as first message
        if let Some(system_blocks) = &request.system {
            let system_text: String = system_blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            if !system_text.is_empty() {
                openai_messages.push(OpenAIMessage {
                    role: "system".to_string(),
                    content: Some(Value::String(system_text)),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }

        // Convert conversation messages
        for msg in request.messages {
            match msg.role {
                MessageRole::User => {
                    // Check if this is a tool_result message
                    let tool_results: Vec<&ContentBlock> = msg
                        .content
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
                        .collect();

                    if !tool_results.is_empty() {
                        // Each tool_result becomes a separate "tool" role message
                        for block in &tool_results {
                            if let ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } = block
                            {
                                let text: String = content
                                    .iter()
                                    .filter_map(|c| match c {
                                        crate::types::ToolResultContentBlock::Text { text } => {
                                            Some(text.as_str())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                openai_messages.push(OpenAIMessage {
                                    role: "tool".to_string(),
                                    content: Some(Value::String(text)),
                                    tool_calls: None,
                                    tool_call_id: Some(tool_use_id.clone()),
                                });
                            }
                        }
                    } else {
                        // Regular user message
                        let text = crate::types::extract_text(msg);
                        openai_messages.push(OpenAIMessage {
                            role: "user".to_string(),
                            content: Some(Value::String(text)),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                }
                MessageRole::Assistant => {
                    let text = crate::types::extract_text(msg);
                    let tool_uses: Vec<OpenAIToolCall> = msg
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, name, input } => Some(OpenAIToolCall {
                                id: id.clone(),
                                call_type: "function".to_string(),
                                function: OpenAIFunctionCall {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(input).unwrap_or_default(),
                                },
                            }),
                            _ => None,
                        })
                        .collect();

                    openai_messages.push(OpenAIMessage {
                        role: "assistant".to_string(),
                        // content 永不为 null：工具调用轮次中模型先输出 tool_calls、
                        // 文本为空，若序列化为 null，Ollama 等兼容端点会以
                        // "invalid message content type: <nil>" 拒绝（400）。
                        // 空字符串是 OpenAI 格式的合法 content。
                        content: Some(Value::String(text)),
                        tool_calls: if tool_uses.is_empty() {
                            None
                        } else {
                            Some(tool_uses)
                        },
                        tool_call_id: None,
                    });
                }
            }
        }

        // Convert tools to OpenAI format
        let openai_tools: Option<Vec<OpenAITool>> = request.tools.map(|tools| {
            tools
                .into_iter()
                .map(|t| OpenAITool {
                    tool_type: "function".to_string(),
                    function: OpenAIFunction {
                        name: t.name,
                        description: t.description,
                        parameters: t.input_schema,
                    },
                })
                .collect()
        });

        let body = OpenAIRequest {
            model: request.model.to_string(),
            max_tokens: request.max_tokens,
            messages: openai_messages,
            tools: openai_tools.filter(|t| !t.is_empty()),
            stream: true,
            think: request.think,
        };

        let mut req_builder = self
            .client
            // base_url 统一按“站点源”处理（可能带 /v1 或不带），
            // 去掉末尾 "/v1" 及尾斜杠后再拼 /v1/chat/completions，
            // 避免 "https://host/v1" 配出 "/v1/v1/chat/completions" 404。
            .post(format!(
                "{}/v1/chat/completions",
                self.base_url.trim_end_matches('/').trim_end_matches("/v1")
            ))
            .header("authorization", format!("Bearer {}", self.api_key))
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
            return Err(ApiError::HttpError {
                status,
                message: text,
            });
        }

        Ok(response)
    }
}

/// Parse the OpenAI SSE byte stream and push chunks into the sender.
async fn parse_openai_stream_into(
    response: reqwest::Response,
    tx: Sender<Result<StreamChunk, ApiError>>,
) {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

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

            let chunk: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if let Some(u) = chunk.get("usage") {
                let mut usage = Usage::default();
                if let Some(pt) = u.get("prompt_tokens").and_then(|v| v.as_u64()) {
                    usage.input_tokens = pt;
                }
                if let Some(ct) = u.get("completion_tokens").and_then(|v| v.as_u64()) {
                    usage.output_tokens = ct;
                }
                let _ = tx.send(Ok(StreamChunk::Usage(usage))).await;
            }

            if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
                for choice in choices {
                    if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                        let sr = match fr {
                            "stop" => "end_turn".to_string(),
                            "tool_calls" => "tool_use".to_string(),
                            "length" => "max_tokens".to_string(),
                            other => other.to_string(),
                        };
                        let _ = tx.send(Ok(StreamChunk::StopReason(sr))).await;
                    }

                    let Some(delta) = choice.get("delta") else {
                        continue;
                    };

                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        let _ = tx
                            .send(Ok(StreamChunk::TextDelta(content.to_string())))
                            .await;
                    }

                    if let Some(reasoning) =
                        delta.get("reasoning_content").and_then(|c| c.as_str())
                    {
                        let _ = tx
                            .send(Ok(StreamChunk::ThinkingDelta(reasoning.to_string())))
                            .await;
                    }

                    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let idx =
                                tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;

                            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
                            let func = tc.get("function");
                            let name = func
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("");
                            let args = func
                                .and_then(|f| f.get("arguments"))
                                .and_then(|a| a.as_str())
                                .unwrap_or("");

                            if !id.is_empty() || !name.is_empty() {
                                let _ = tx
                                    .send(Ok(StreamChunk::ToolUseStart {
                                        index: idx,
                                        id: id.to_string(),
                                        name: name.to_string(),
                                    }))
                                    .await;
                            }
                            if !args.is_empty() {
                                let _ = tx
                                    .send(Ok(StreamChunk::ToolUseDelta {
                                        index: idx,
                                        partial_json: args.to_string(),
                                    }))
                                    .await;
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = tx.send(Ok(StreamChunk::Done)).await;
}

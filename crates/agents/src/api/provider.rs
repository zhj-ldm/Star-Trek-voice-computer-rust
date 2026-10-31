use crate::types::{ApiToolParam, Message, SystemBlock, ThinkingConfig, Usage};
use async_trait::async_trait;
use tokio::sync::mpsc::Receiver;

/// API type identifier.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiType {
    AnthropicMessages,
    OpenAICompletions,
}

/// A single delta emitted during a streaming LLM response.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// Incremental text output from the assistant.
    TextDelta(String),
    /// Incremental thinking/reasoning content.
    ThinkingDelta(String),
    /// A new tool-use block started (id + name known, input still streaming).
    ToolUseStart {
        index: usize,
        id: String,
        name: String,
    },
    /// Partial JSON arguments for an in-progress tool use.
    ToolUseDelta {
        index: usize,
        partial_json: String,
    },
    /// Token usage (usually emitted at the end).
    Usage(Usage),
    /// The stop reason reported by the model.
    StopReason(String),
    /// Stream finished.
    Done,
}

/// Normalized response from any LLM provider.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub message: Message,
    pub usage: Usage,
    pub stop_reason: Option<String>,
}

/// Configuration passed to provider for each request.
#[derive(Debug, Clone)]
pub struct ProviderRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u64,
    pub messages: &'a [Message],
    pub system: Option<Vec<SystemBlock>>,
    pub tools: Option<Vec<ApiToolParam>>,
    pub thinking: Option<ThinkingConfig>,
    /// 模型思考开关（仅本地 Ollama 等支持 think 参数的上游使用；None=不传）
    pub think: Option<bool>,
}

/// Trait that all LLM providers must implement.
#[async_trait]
pub trait LLMProvider: Send + Sync {
    fn api_type(&self) -> ApiType;

    async fn create_message(
        &self,
        request: ProviderRequest<'_>,
    ) -> Result<ProviderResponse, super::ApiError>;

    /// Stream the response, yielding incremental chunks.
    /// Default implementation falls back to non-streaming and emits the full
    /// text as a single delta; providers should override for true streaming.
    async fn create_message_stream(
        &self,
        request: ProviderRequest<'_>,
    ) -> Result<Receiver<Result<StreamChunk, super::ApiError>>, super::ApiError> {
        let resp = self.create_message(request).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            let text: String = resp
                .message
                .content
                .iter()
                .filter_map(|b| match b {
                    crate::types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if !text.is_empty() {
                let _ = tx.send(Ok(StreamChunk::TextDelta(text))).await;
            }
            let _ = tx.send(Ok(StreamChunk::Usage(resp.usage))).await;
            if let Some(sr) = resp.stop_reason {
                let _ = tx.send(Ok(StreamChunk::StopReason(sr))).await;
            }
            let _ = tx.send(Ok(StreamChunk::Done)).await;
        });
        Ok(rx)
    }
}

/// Detect API type from model name using heuristics (matches TS SDK).
pub fn detect_api_type(model: &str, env_api_type: Option<&str>) -> ApiType {
    // 1. Explicit env override
    if let Some(t) = env_api_type {
        match t {
            "openai-completions" => return ApiType::OpenAICompletions,
            "anthropic-messages" => return ApiType::AnthropicMessages,
            _ => {}
        }
    }

    // 2. Also check CODEANY_API_TYPE env var
    if let Ok(t) = std::env::var("CODEANY_API_TYPE") {
        match t.as_str() {
            "openai-completions" => return ApiType::OpenAICompletions,
            "anthropic-messages" => return ApiType::AnthropicMessages,
            _ => {}
        }
    }

    // 3. Model name heuristics
    let m = model.to_lowercase();
    if m.contains("gpt-")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.contains("deepseek")
        || m.contains("qwen")
        || m.contains("yi-")
        || m.contains("glm")
        || m.contains("mistral")
        || m.contains("gemma")
        || m.contains("mimo")
        || m.contains("llama")
        || m.contains("gemini")
    {
        return ApiType::OpenAICompletions;
    }

    // Default: Anthropic
    ApiType::AnthropicMessages
}

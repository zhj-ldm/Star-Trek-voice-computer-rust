use crate::types::{
    ApiToolParam, Message, SystemBlock, ThinkingConfig, Usage,
};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::anthropic::AnthropicProvider;
use super::openai::OpenAIProvider;
use super::provider::{detect_api_type, ApiType, LLMProvider, ProviderRequest, ProviderResponse};
use super::rpm::{DEFAULT_RPM, RpmLimiter};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_TIMEOUT_MS: u64 = 600_000; // 10 minutes

/// Model configuration with context window and output limits.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub context_window: u64,
    pub max_output_tokens: u64,
}

/// Get model configuration for a given model ID.
pub fn get_model_config(model: &str) -> ModelConfig {
    match model {
        m if m.contains("opus") => ModelConfig {
            context_window: 200_000,
            max_output_tokens: 32_000,
        },
        m if m.contains("sonnet") => ModelConfig {
            context_window: 200_000,
            max_output_tokens: 16_000,
        },
        m if m.contains("haiku") => ModelConfig {
            context_window: 200_000,
            max_output_tokens: 8_192,
        },
        m if m.starts_with("gpt-4o") => ModelConfig {
            context_window: 128_000,
            max_output_tokens: 16_384,
        },
        m if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") => ModelConfig {
            context_window: 200_000,
            max_output_tokens: 100_000,
        },
        m if m.contains("deepseek") => ModelConfig {
            context_window: 128_000,
            max_output_tokens: 8_192,
        },
        _ => ModelConfig {
            context_window: 200_000,
            max_output_tokens: 16_000,
        },
    }
}

/// Streaming event from the API (kept for backward compat).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub message: Option<Value>,
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub content_block: Option<Value>,
    #[serde(default)]
    pub delta: Option<Value>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// Complete API response (non-streaming).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ApiResponse {
    pub id: String,
    pub content: Vec<Value>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub usage: Usage,
}

/// Error from the API.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP error: {status} - {message}")]
    HttpError { status: u16, message: String },

    #[error("Authentication error: {0}")]
    AuthError(String),

    #[error("Rate limit exceeded")]
    RateLimitError,

    #[error("Prompt too long: {0}")]
    PromptTooLong(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Request timeout")]
    Timeout,
}

/// API client that auto-detects and delegates to the correct provider.
#[derive(Clone)]
pub struct ApiClient {
    provider: Arc<dyn LLMProvider>,
    model: String,
    api_type: ApiType,
    /// 每分钟请求数限流器（每个客户端独立，默认 20 RPM）
    rpm_limiter: Arc<RpmLimiter>,
    /// 模型思考开关（默认关闭；仅对本地 Ollama/llama 上游生效，远端 API 不传）
    enable_thinking: bool,
    /// 上游是否为本地地址（127.0.0.1 / localhost）
    is_local: bool,
}

impl ApiClient {
    pub fn new(api_key: Option<String>, base_url: Option<String>, model: Option<String>) -> Self {
        Self::with_api_type(api_key, base_url, model, None, None)
    }

    /// Create a client with an explicit API type override.
    pub fn with_api_type(
        api_key: Option<String>,
        base_url: Option<String>,
        model: Option<String>,
        api_type: Option<ApiType>,
        rpm_limit: Option<u32>,
    ) -> Self {
        let api_key = api_key
            .or_else(|| std::env::var("CODEANY_API_KEY").ok())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .unwrap_or_default();

        let base_url = base_url
            .or_else(|| std::env::var("CODEANY_BASE_URL").ok())
            .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let model = model
            .or_else(|| std::env::var("CODEANY_MODEL").ok())
            .or_else(|| std::env::var("ANTHROPIC_MODEL").ok())
            .unwrap_or_else(|| "sonnet-4-6".to_string());

        let timeout_ms: u64 = std::env::var("API_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS);

        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .expect("Failed to create HTTP client");

        let mut custom_headers = HashMap::new();
        if let Ok(headers_str) = std::env::var("CODEANY_CUSTOM_HEADERS") {
            for pair in headers_str.split(',') {
                if let Some((key, value)) = pair.split_once(':') {
                    custom_headers.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
        }

        // Detect API type
        let resolved_api_type = api_type.unwrap_or_else(|| detect_api_type(&model, None));

        // 上游是否为本地地址（在 base_url 被移入 provider 前计算）
        let is_local = base_url.contains("127.0.0.1") || base_url.contains("localhost");

        let provider: Arc<dyn LLMProvider> = match &resolved_api_type {
            ApiType::AnthropicMessages => Arc::new(AnthropicProvider::new(
                client,
                api_key,
                base_url,
                custom_headers,
            )),
            ApiType::OpenAICompletions => Arc::new(OpenAIProvider::new(
                client,
                api_key,
                base_url,
                custom_headers,
            )),
        };

        Self {
            provider,
            model,
            api_type: resolved_api_type,
            rpm_limiter: Arc::new(RpmLimiter::new(rpm_limit.unwrap_or_else(|| {
                std::env::var("RPM_LIMIT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_RPM)
            }))),
            enable_thinking: false,
            is_local,
        }
    }

    /// 覆盖模型思考开关（默认关闭）。仅本地上游会携带 think 参数。
    pub fn set_enable_thinking(&mut self, enable: bool) {
        self.enable_thinking = enable;
    }

    /// 覆盖每分钟请求数限制（仅在构建 Agent 前调用）。
    pub fn set_rpm_limit(&mut self, rpm: u32) {
        self.rpm_limiter = Arc::new(RpmLimiter::new(rpm));
    }

    /// 当前 RPM 限速值。
    pub fn rpm_limit(&self) -> u32 {
        self.rpm_limiter.rpm()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: String) {
        self.model = model;
    }

    pub fn api_type(&self) -> &ApiType {
        &self.api_type
    }

    pub fn model_config(&self) -> ModelConfig {
        get_model_config(&self.model)
    }

    /// Send a streaming request via the provider and return the parsed response.
    pub async fn create_message(
        &self,
        messages: &[Message],
        system: Option<Vec<SystemBlock>>,
        tools: Option<Vec<ApiToolParam>>,
        max_tokens: Option<u64>,
        thinking: Option<ThinkingConfig>,
    ) -> Result<ProviderResponse, ApiError> {
        let model_config = self.model_config();
        let max_tokens = max_tokens.unwrap_or(model_config.max_output_tokens);

        // 思考开关：仅本地 Ollama/llama 上游显式携带 think 参数（远端 API 可能拒绝未知字段）
        let think = if self.is_local { Some(self.enable_thinking) } else { None };

        let request = ProviderRequest {
            model: &self.model,
            max_tokens,
            messages,
            system,
            tools,
            thinking,
            think,
        };

        // RPM 限速：超过每分钟配额时在客户端侧等待，避免上游 429/连接被拒
        self.rpm_limiter.acquire().await;

        self.provider.create_message(request).await
    }

    /// Legacy method: send streaming request and return raw reqwest::Response.
    /// Delegates to the Anthropic provider only; prefer `create_message()` instead.
    pub async fn create_message_stream(
        &self,
        _messages: &[Message],
        _system: Option<Vec<SystemBlock>>,
        _tools: Option<Vec<ApiToolParam>>,
        _max_tokens: Option<u64>,
        _thinking: Option<ThinkingConfig>,
    ) -> Result<reqwest::Response, ApiError> {
        // For backward compat, this method is only meaningful for direct SSE parsing.
        // New code should use create_message() which handles both providers.
        Err(ApiError::ParseError(
            "create_message_stream is deprecated; use create_message() instead".to_string(),
        ))
    }

    /// Legacy: Parse Anthropic SSE stream. Kept for backward compatibility.
    pub async fn parse_stream(
        _response: reqwest::Response,
    ) -> Result<(Message, Usage, Option<String>), ApiError> {
        let _pr = super::anthropic::AnthropicProvider::new(
            Client::new(),
            String::new(),
            String::new(),
            HashMap::new(),
        );
        // Redirect to new provider-based parsing isn't possible with raw response.
        // This method is kept only for API compat but new code should use create_message().
        Err(ApiError::ParseError(
            "parse_stream is deprecated; use create_message() instead".to_string(),
        ))
    }
}

/// Check if an error is retryable.
pub fn is_retryable_error(error: &ApiError) -> bool {
    matches!(
        error,
        ApiError::RateLimitError
            | ApiError::Timeout
            | ApiError::NetworkError(_)
            | ApiError::HttpError { status: 500..=599, .. }
    )
}

/// Check if an error is an auth error.
pub fn is_auth_error(error: &ApiError) -> bool {
    matches!(error, ApiError::AuthError(_))
}

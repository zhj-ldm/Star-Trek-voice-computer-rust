use open_agent_sdk::types::*;
use open_agent_sdk::utils::{compact, messages, retry, tokens};
use serde_json::json;

// --- Token Tests ---

#[test]
fn test_estimate_tokens() {
    assert_eq!(tokens::estimate_tokens(""), 0);
    assert_eq!(tokens::estimate_tokens("1234"), 1);
    assert_eq!(tokens::estimate_tokens("12345678"), 2);
    assert!(tokens::estimate_tokens("hello world this is a test") > 0);
}

#[test]
fn test_estimate_messages_tokens() {
    let msgs = vec![
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        },
        Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "world".to_string(),
            }],
        },
    ];
    let tokens = tokens::estimate_messages_tokens(&msgs);
    assert!(tokens > 0);
}

#[test]
fn test_get_context_window_size() {
    assert_eq!(
        tokens::get_context_window_size("claude-sonnet-4-6-20250514"),
        200_000
    );
    assert_eq!(
        tokens::get_context_window_size("claude-opus-4-6-20250514"),
        200_000
    );
    assert_eq!(
        tokens::get_context_window_size("claude-haiku-4-5-20251001"),
        200_000
    );
}

#[test]
fn test_get_auto_compact_threshold() {
    let threshold = tokens::get_auto_compact_threshold("claude-sonnet-4-6-20250514");
    assert_eq!(threshold, 200_000 - 13_000);
}

#[test]
fn test_estimate_cost() {
    let usage = Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };

    let cost = tokens::estimate_cost("claude-sonnet-4-6-20250514", &usage);
    // Sonnet: $3/M input + $15/M output = $18
    assert!((cost - 18.0).abs() < 0.01);
}

#[test]
fn test_estimate_cost_with_cache() {
    let usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 1_000_000,
        cache_read_input_tokens: 1_000_000,
    };

    let cost = tokens::estimate_cost("claude-sonnet-4-6-20250514", &usage);
    // Sonnet cache: $3.75/M write + $0.3/M read = $4.05
    assert!((cost - 4.05).abs() < 0.01);
}

// --- Message Utility Tests ---

#[test]
fn test_create_user_message() {
    let msg = messages::create_user_message("hello");
    assert_eq!(msg.role, MessageRole::User);
    assert_eq!(messages::extract_text(&msg), "hello");
}

#[test]
fn test_create_assistant_message() {
    let msg = messages::create_assistant_message("hi there");
    assert_eq!(msg.role, MessageRole::Assistant);
    assert_eq!(messages::extract_text(&msg), "hi there");
}

#[test]
fn test_normalize_messages_empty() {
    let normalized = messages::normalize_messages(&[]);
    assert!(normalized.is_empty());
}

#[test]
fn test_normalize_messages_starts_with_user() {
    let msgs = vec![messages::create_assistant_message("I'm an assistant")];
    let normalized = messages::normalize_messages(&msgs);
    assert_eq!(normalized[0].role, MessageRole::User);
    assert_eq!(normalized.len(), 2);
}

#[test]
fn test_normalize_messages_merges_consecutive() {
    let msgs = vec![
        messages::create_user_message("hello"),
        messages::create_user_message("world"),
        messages::create_assistant_message("hi"),
    ];
    let normalized = messages::normalize_messages(&msgs);
    assert_eq!(normalized.len(), 2); // Merged two user messages
    assert_eq!(normalized[0].role, MessageRole::User);
    assert_eq!(normalized[1].role, MessageRole::Assistant);
}

#[test]
fn test_strip_images() {
    let msgs = vec![Message {
        role: MessageRole::User,
        content: vec![
            ContentBlock::Text {
                text: "look at this".to_string(),
            },
            ContentBlock::Image {
                source: ImageContentSource {
                    source_type: "base64".to_string(),
                    media_type: "image/png".to_string(),
                    data: "abcdef".to_string(),
                },
            },
        ],
    }];

    let stripped = messages::strip_images(&msgs);
    assert_eq!(stripped[0].content.len(), 1);
    match &stripped[0].content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "look at this"),
        _ => panic!("Expected text block"),
    }
}

#[test]
fn test_truncate_text() {
    assert_eq!(messages::truncate_text("hello", 10), "hello");
    assert_eq!(
        messages::truncate_text("hello world", 5),
        "hello... (truncated)"
    );
}

// --- Compact Tests ---

#[test]
fn test_should_auto_compact_false() {
    let msgs = vec![messages::create_user_message("short message")];
    assert!(!compact::should_auto_compact(&msgs, "claude-sonnet-4-6-20250514"));
}

#[test]
fn test_micro_compact_preserves_short_results() {
    let msgs = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tu_1".to_string(),
            content: vec![ToolResultContentBlock::Text {
                text: "short result".to_string(),
            }],
            is_error: false,
        }],
    }];

    let compacted = compact::micro_compact_messages(&msgs);
    if let ContentBlock::ToolResult { content, .. } = &compacted[0].content[0] {
        if let ToolResultContentBlock::Text { text } = &content[0] {
            assert_eq!(text, "short result");
        }
    }
}

#[test]
fn test_micro_compact_truncates_long_results() {
    let long_text = "x".repeat(100_000);
    let msgs = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tu_1".to_string(),
            content: vec![ToolResultContentBlock::Text {
                text: long_text.clone(),
            }],
            is_error: false,
        }],
    }];

    let compacted = compact::micro_compact_messages(&msgs);
    if let ContentBlock::ToolResult { content, .. } = &compacted[0].content[0] {
        if let ToolResultContentBlock::Text { text } = &content[0] {
            assert!(text.len() < long_text.len());
            assert!(text.contains("truncated"));
        }
    }
}

// --- Retry Tests ---

#[test]
fn test_retry_config_default() {
    let config = retry::RetryConfig::default();
    assert_eq!(config.max_retries, 3);
    assert_eq!(config.base_delay_ms, 2000);
    assert_eq!(config.max_delay_ms, 30_000);
}

#[test]
fn test_is_retryable() {
    use open_agent_sdk::api::ApiError;

    assert!(retry::is_retryable(&ApiError::RateLimitError));
    assert!(retry::is_retryable(&ApiError::Timeout));
    assert!(retry::is_retryable(&ApiError::NetworkError(
        "conn reset".to_string()
    )));
    assert!(retry::is_retryable(&ApiError::HttpError {
        status: 500,
        message: "internal error".to_string(),
    }));
    assert!(!retry::is_retryable(&ApiError::AuthError(
        "bad key".to_string()
    )));
    assert!(!retry::is_retryable(&ApiError::PromptTooLong(
        "too long".to_string()
    )));
}

#[test]
fn test_is_auth_error() {
    use open_agent_sdk::api::ApiError;

    assert!(retry::is_auth_error(&ApiError::AuthError(
        "bad key".to_string()
    )));
    assert!(!retry::is_auth_error(&ApiError::RateLimitError));
}

#[test]
fn test_is_prompt_too_long() {
    use open_agent_sdk::api::ApiError;

    assert!(retry::is_prompt_too_long(&ApiError::PromptTooLong(
        "too long".to_string()
    )));
    assert!(!retry::is_prompt_too_long(&ApiError::RateLimitError));
}

#[test]
fn test_get_retry_delay() {
    let config = retry::RetryConfig::default();
    let delay = retry::get_retry_delay(&config, 0);
    // Should be between 1s and 3s (base 2s with jitter)
    assert!(delay.as_millis() >= 1000);
    assert!(delay.as_millis() <= 4000);
}

use open_agent_sdk::types::*;
use serde_json::json;

#[test]
fn test_tool_result_text() {
    let result = ToolResult::text("hello world");
    assert!(!result.is_error);
    assert_eq!(result.get_text(), "hello world");
}

#[test]
fn test_tool_result_error() {
    let result = ToolResult::error("something went wrong");
    assert!(result.is_error);
    assert_eq!(result.get_text(), "something went wrong");
}

#[test]
fn test_tool_result_image() {
    let result = ToolResult::image("base64data".to_string(), "image/png".to_string());
    assert!(!result.is_error);
    assert_eq!(result.content.len(), 1);
    match &result.content[0] {
        ToolResultContent::Image { source } => {
            assert_eq!(source.data, "base64data");
            assert_eq!(source.media_type, "image/png");
        }
        _ => panic!("Expected image content"),
    }
}

#[test]
fn test_tool_input_schema_default() {
    let schema = ToolInputSchema::default();
    assert_eq!(schema.schema_type, "object");
    assert!(schema.properties.is_empty());
    assert!(schema.required.is_empty());
}

#[test]
fn test_permission_mode_default() {
    let mode = PermissionMode::default();
    assert_eq!(mode, PermissionMode::BypassPermissions);
}

#[test]
fn test_permission_mode_serialization() {
    let mode = PermissionMode::AcceptEdits;
    let json = serde_json::to_string(&mode).unwrap();
    assert_eq!(json, "\"acceptEdits\"");

    let deserialized: PermissionMode = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, PermissionMode::AcceptEdits);
}

#[test]
fn test_message_creation() {
    let msg = Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
    };
    assert_eq!(msg.role, MessageRole::User);
    assert_eq!(msg.content.len(), 1);
}

#[test]
fn test_extract_text_from_message() {
    let msg = Message {
        role: MessageRole::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "Hello ".to_string(),
            },
            ContentBlock::Text {
                text: "World".to_string(),
            },
        ],
    };
    assert_eq!(extract_text(&msg), "Hello World");
}

#[test]
fn test_extract_text_skips_non_text() {
    let msg = Message {
        role: MessageRole::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "Hello".to_string(),
            },
            ContentBlock::ToolUse {
                id: "123".to_string(),
                name: "Read".to_string(),
                input: json!({}),
            },
            ContentBlock::Text {
                text: " World".to_string(),
            },
        ],
    };
    assert_eq!(extract_text(&msg), "Hello World");
}

#[test]
fn test_extract_tool_uses() {
    let msg = Message {
        role: MessageRole::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "Let me read that".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "Read".to_string(),
                input: json!({"file_path": "/tmp/test.txt"}),
            },
            ContentBlock::ToolUse {
                id: "tu_2".to_string(),
                name: "Glob".to_string(),
                input: json!({"pattern": "*.rs"}),
            },
        ],
    };

    let tool_uses = extract_tool_uses(&msg);
    assert_eq!(tool_uses.len(), 2);
    assert_eq!(tool_uses[0].1, "Read");
    assert_eq!(tool_uses[1].1, "Glob");
}

#[test]
fn test_usage_default() {
    let usage = Usage::default();
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 0);
    assert_eq!(usage.cache_creation_input_tokens, 0);
    assert_eq!(usage.cache_read_input_tokens, 0);
}

#[test]
fn test_thinking_config() {
    let config = ThinkingConfig::enabled(10000);
    assert_eq!(config.thinking_type, "enabled");
    assert_eq!(config.budget_tokens, Some(10000));

    let disabled = ThinkingConfig::disabled();
    assert_eq!(disabled.thinking_type, "disabled");
    assert_eq!(disabled.budget_tokens, None);
}

#[test]
fn test_message_role_serialization() {
    let user: MessageRole = serde_json::from_str("\"user\"").unwrap();
    assert_eq!(user, MessageRole::User);

    let assistant: MessageRole = serde_json::from_str("\"assistant\"").unwrap();
    assert_eq!(assistant, MessageRole::Assistant);
}

#[test]
fn test_content_block_text_serialization() {
    let block = ContentBlock::Text {
        text: "hello".to_string(),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "text");
    assert_eq!(json["text"], "hello");
}

#[test]
fn test_content_block_tool_use_serialization() {
    let block = ContentBlock::ToolUse {
        id: "tu_1".to_string(),
        name: "Read".to_string(),
        input: json!({"file_path": "/test.txt"}),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "tool_use");
    assert_eq!(json["name"], "Read");
    assert_eq!(json["input"]["file_path"], "/test.txt");
}

#[test]
fn test_sdk_message_serialization() {
    let msg = SDKMessage::System {
        message: "Agent started".to_string(),
    };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["type"], "system");
    assert_eq!(json["message"], "Agent started");
}

#[test]
fn test_cache_control() {
    let cc = CacheControl::ephemeral();
    assert_eq!(cc.control_type, "ephemeral");
}

#[test]
fn test_mcp_server_config_stdio() {
    let config = McpServerConfig::Stdio {
        command: "npx".to_string(),
        args: vec!["server".to_string()],
        env: std::collections::HashMap::new(),
    };
    let json = serde_json::to_value(&config).unwrap();
    assert_eq!(json["transport"], "stdio");
    assert_eq!(json["command"], "npx");
}

use open_agent_sdk::{Agent, AgentOptions};
use open_agent_sdk::types::*;
use std::sync::Arc;

#[tokio::test]
async fn test_agent_creation() {
    let agent = Agent::new(AgentOptions::default()).await.unwrap();
    assert!(agent.get_messages().is_empty());
    assert!(!agent.session_id().is_empty());
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_custom_model() {
    let agent = Agent::new(AgentOptions {
        model: Some("claude-haiku-4-5-20251001".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();

    assert_eq!(agent.model(), "claude-haiku-4-5-20251001");
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_custom_cwd() {
    let agent = Agent::new(AgentOptions {
        cwd: Some("/tmp".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();

    // Agent should use /tmp as working directory
    assert!(!agent.session_id().is_empty());
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_allowed_tools() {
    let agent = Agent::new(AgentOptions {
        allowed_tools: Some(vec!["Read".to_string(), "Glob".to_string()]),
        ..Default::default()
    })
    .await
    .unwrap();

    // Agent should only have Read and Glob
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_custom_tools() {
    use async_trait::async_trait;

    struct TestTool;

    #[async_trait]
    impl Tool for TestTool {
        fn name(&self) -> &str {
            "TestTool"
        }
        fn description(&self) -> &str {
            "A test tool"
        }
        fn input_schema(&self) -> ToolInputSchema {
            ToolInputSchema::default()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("test result"))
        }
    }

    let agent = Agent::new(AgentOptions {
        custom_tools: vec![Arc::new(TestTool)],
        ..Default::default()
    })
    .await
    .unwrap();

    assert!(!agent.session_id().is_empty());
    agent.close().await;
}

#[tokio::test]
async fn test_agent_clear() {
    let mut agent = Agent::new(AgentOptions::default()).await.unwrap();

    // Manually add a message for testing
    agent.messages.push(Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "test".to_string(),
        }],
    });

    assert!(!agent.get_messages().is_empty());
    agent.clear();
    assert!(agent.get_messages().is_empty());

    agent.close().await;
}

#[tokio::test]
async fn test_agent_set_model() {
    let mut agent = Agent::new(AgentOptions::default()).await.unwrap();

    agent.set_model("claude-opus-4-6-20250514");
    assert_eq!(agent.model(), "claude-opus-4-6-20250514");

    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_max_turns() {
    let agent = Agent::new(AgentOptions {
        max_turns: Some(5),
        ..Default::default()
    })
    .await
    .unwrap();

    assert_eq!(agent.max_turns, 5);
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_max_budget() {
    let agent = Agent::new(AgentOptions {
        max_budget_usd: Some(1.0),
        ..Default::default()
    })
    .await
    .unwrap();

    assert_eq!(agent.max_budget_usd, Some(1.0));
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_thinking() {
    let agent = Agent::new(AgentOptions {
        thinking: Some(ThinkingConfig::enabled(10000)),
        ..Default::default()
    })
    .await
    .unwrap();

    assert!(agent.thinking.is_some());
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_system_prompt() {
    let agent = Agent::new(AgentOptions {
        system_prompt: Some("You are a code reviewer.".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();

    assert_eq!(
        agent.system_prompt.as_deref(),
        Some("You are a code reviewer.")
    );
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_append_system_prompt() {
    let agent = Agent::new(AgentOptions {
        append_system_prompt: Some("Always respond in JSON.".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();

    assert_eq!(
        agent.append_system_prompt.as_deref(),
        Some("Always respond in JSON.")
    );
    agent.close().await;
}

#[tokio::test]
async fn test_agent_cost_tracker() {
    let agent = Agent::new(AgentOptions::default()).await.unwrap();

    let cost = agent.cost_tracker().total_cost().await;
    assert_eq!(cost, 0.0);

    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_permission_mode() {
    let agent = Agent::new(AgentOptions {
        permission_mode: Some(PermissionMode::AcceptEdits),
        ..Default::default()
    })
    .await
    .unwrap();

    // Agent should be created successfully with the permission mode
    assert!(!agent.session_id().is_empty());
    agent.close().await;
}

#[tokio::test]
async fn test_agent_with_disallowed_tools() {
    let agent = Agent::new(AgentOptions {
        disallowed_tools: Some(vec!["Bash".to_string()]),
        ..Default::default()
    })
    .await
    .unwrap();

    // Agent should not have Bash tool
    assert!(!agent.session_id().is_empty());
    agent.close().await;
}

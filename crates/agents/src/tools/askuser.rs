use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

/// Callback function for asking the user a question.
pub type AskUserFn =
    Arc<dyn Fn(&str) -> futures::future::BoxFuture<'static, Result<String, String>> + Send + Sync>;

pub struct AskUserTool {
    ask_fn: Option<AskUserFn>,
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self { ask_fn: None }
    }
}

impl AskUserTool {
    pub fn new(ask_fn: AskUserFn) -> Self {
        Self {
            ask_fn: Some(ask_fn),
        }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their response. Use when you need clarification or input."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([(
                "question".to_string(),
                json!({
                    "type": "string",
                    "description": "The question to ask the user"
                }),
            )]),
            required: vec!["question".to_string()],
            additional_properties: Some(false),
        }
    }

    async fn call(&self, input: Value, _context: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let question = input
            .get("question")
            .and_then(|q| q.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'question' field".to_string()))?;

        match &self.ask_fn {
            Some(ask_fn) => {
                let answer = (ask_fn)(question)
                    .await
                    .map_err(|e| ToolError::ExecutionError(e))?;
                Ok(ToolResult::text(answer))
            }
            None => Ok(ToolResult::error(
                "User interaction is not configured. Provide an ask_fn when creating the agent."
                    .to_string(),
            )),
        }
    }
}

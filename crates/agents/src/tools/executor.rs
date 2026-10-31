use crate::types::{
    CanUseToolFn, ContentBlock, Message, MessageRole, PermissionDecision,
    Tool, ToolError, ToolResult, ToolResultContentBlock, ToolUseContext,
};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::registry::ToolRegistry;

/// Execute a set of tool calls from an assistant message.
/// Concurrent-safe tools run in parallel; others run sequentially.
/// `cancel` token (if any) is checked before each tool invocation to allow
/// interruption (语音打断 / 用户中断).
pub async fn execute_tools(
    message: &Message,
    registry: &ToolRegistry,
    context: &ToolUseContext,
    permission_fn: Option<&CanUseToolFn>,
    cancel: Option<CancellationToken>,
) -> Vec<(String, String, ToolResult)> {
    let tool_uses: Vec<(String, String, Value)> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect();

    if tool_uses.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();

    // Run concurrent tools in parallel
    let mut handles = Vec::new();
    let concurrent_calls: Vec<(String, String, Value, Arc<dyn Tool>)> = tool_uses
        .iter()
        .filter_map(|(id, name, input)| {
            registry
                .get(name)
                .filter(|t| t.is_concurrency_safe(input))
                .map(|t| (id.clone(), name.clone(), input.clone(), t.clone()))
        })
        .collect();

    for (id, name, input, tool) in &concurrent_calls {
        if let Some(ref c) = cancel {
            if c.is_cancelled() {
                break;
            }
        }
        let ctx = context.clone();
        let perm_fn = permission_fn.cloned();
        let tool = tool.clone();
        let cancel_clone = cancel.clone();
        let id = id.clone();
        let name = name.clone();
        let input = input.clone();
        handles.push(tokio::spawn(async move {
            if let Some(ref c) = cancel_clone {
                if c.is_cancelled() {
                    return (
                        id,
                        name,
                        ToolResult::error("Interrupted before tool execution"),
                    );
                }
            }
            let input = check_permission(&name, input, perm_fn.as_ref());
            match input {
                Ok(input) => {
                    let result = tool.call(input, &ctx).await;
                    let tool_result = match result {
                        Ok(r) => r,
                        Err(e) => ToolResult::error(e.to_string()),
                    };
                    (id, name, tool_result)
                }
                Err(msg) => (id, name, ToolResult::error(msg)),
            }
        }));
    }

    for handle in handles {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }

    // Run sequential tools one at a time
    for (id, name, input) in tool_uses.iter().filter(|(_, n, input)| {
        // Not concurrent-safe
        registry
            .get(n)
            .map(|t| !t.is_concurrency_safe(input))
            .unwrap_or(true)
    }) {
        if let Some(ref c) = cancel {
            if c.is_cancelled() {
                results.push((
                    id.clone(),
                    name.clone(),
                    ToolResult::error("Interrupted before tool execution"),
                ));
                continue;
            }
        }
        let checked_input = check_permission(name, input.clone(), permission_fn);
        match checked_input {
            Ok(ci) => {
                let result = tool_call_with_cancel(registry, name, ci, context, cancel.clone()).await;
                results.push((id.clone(), name.clone(), result));
            }
            Err(msg) => {
                results.push((id.clone(), name.clone(), ToolResult::error(msg)));
            }
        }
    }

    results
}

async fn tool_call_with_cancel(
    registry: &ToolRegistry,
    name: &str,
    input: Value,
    context: &ToolUseContext,
    cancel: Option<CancellationToken>,
) -> ToolResult {
    let Some(tool) = registry.get(name) else {
        return ToolResult::error(format!("Unknown tool: {}", name));
    };

    let tool_fut = tool.call(input.clone(), context);
    tokio::select! {
        r = tool_fut => match r {
            Ok(r) => r,
            Err(e) => ToolResult::error(e.to_string()),
        },
        _ = async {
            if let Some(c) = cancel { c.cancelled().await } else { std::future::pending::<()>().await }
        } => ToolResult::error("Tool interrupted"),
    }
}

fn check_permission(
    tool_name: &str,
    input: Value,
    permission_fn: Option<&CanUseToolFn>,
) -> Result<Value, String> {
    if let Some(perm_fn) = permission_fn {
        match perm_fn(tool_name, &input) {
            PermissionDecision::Allow => Ok(input),
            PermissionDecision::Deny(msg) => Err(msg),
            PermissionDecision::AllowWithModifiedInput(new_input) => Ok(new_input),
        }
    } else {
        Ok(input)
    }
}

/// Build a user message containing tool results.
pub fn build_tool_results_message(results: &[(String, String, ToolResult)]) -> Message {
    let content: Vec<ContentBlock> = results
        .iter()
        .map(|(id, _name, result)| {
            let content_blocks: Vec<ToolResultContentBlock> = result
                .content
                .iter()
                .map(|c| match c {
                    crate::types::ToolResultContent::Text { text } => {
                        ToolResultContentBlock::Text {
                            text: text.clone(),
                        }
                    }
                    crate::types::ToolResultContent::Image { source } => {
                        ToolResultContentBlock::Image {
                            source: crate::types::ImageContentSource {
                                source_type: source.source_type.clone(),
                                media_type: source.media_type.clone(),
                                data: source.data.clone(),
                            },
                        }
                    }
                })
                .collect();

            ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: content_blocks,
                is_error: result.is_error,
            }
        })
        .collect();

    Message {
        role: MessageRole::User,
        content,
    }
}

/// Placeholder tool for unknown tool names.
struct UnknownTool(String);

#[async_trait::async_trait]
impl Tool for UnknownTool {
    fn name(&self) -> &str {
        &self.0
    }

    fn description(&self) -> &str {
        "Unknown tool"
    }

    fn input_schema(&self) -> crate::types::ToolInputSchema {
        crate::types::ToolInputSchema::default()
    }

    async fn call(
        &self,
        _input: Value,
        _context: &ToolUseContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::error(format!(
            "Unknown tool: {}. Use ToolSearch to discover available tools.",
            self.0
        )))
    }
}

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::types::{McpContentBlock, McpToolDefinition, Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};
use super::client::McpClient;

/// Wraps an MCP tool as a standard Tool implementation.
/// Tool names are prefixed: mcp__{server_name}__{tool_name}
pub struct McpToolWrapper {
    server_name: String,
    tool_def: McpToolDefinition,
    client: Arc<McpClient>,
    prefixed_name: String,
}

impl McpToolWrapper {
    pub fn new(
        server_name: &str,
        tool_def: McpToolDefinition,
        client: Arc<McpClient>,
    ) -> Self {
        let prefixed_name = format!("mcp__{}_{}", server_name, tool_def.name);
        Self {
            server_name: server_name.to_string(),
            tool_def,
            client,
            prefixed_name,
        }
    }
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.prefixed_name
    }

    fn description(&self) -> &str {
        self.tool_def
            .description
            .as_deref()
            .unwrap_or("MCP tool")
    }

    fn input_schema(&self) -> ToolInputSchema {
        if let Some(schema) = &self.tool_def.input_schema {
            if let Ok(parsed) = serde_json::from_value::<ToolInputSchema>(schema.clone()) {
                return parsed;
            }
        }
        ToolInputSchema::default()
    }

    async fn call(
        &self,
        input: Value,
        _context: &ToolUseContext,
    ) -> Result<ToolResult, ToolError> {
        let result = self
            .client
            .call_tool(&self.server_name, &self.tool_def.name, input)
            .await
            .map_err(|e| ToolError::ExecutionError(e))?;

        if result.is_error {
            let text: String = result
                .content
                .iter()
                .filter_map(|c| match c {
                    McpContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(ToolResult::error(text))
        } else {
            let text: String = result
                .content
                .iter()
                .filter_map(|c| match c {
                    McpContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(ToolResult::text(text))
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false // MCP tools are treated as mutations by default
    }
}

/// Create tool wrappers for all tools from a connected MCP server.
pub fn create_mcp_tools(
    server_name: &str,
    tools: &[McpToolDefinition],
    client: Arc<McpClient>,
) -> Vec<Arc<dyn Tool>> {
    tools
        .iter()
        .map(|tool_def| {
            Arc::new(McpToolWrapper::new(server_name, tool_def.clone(), client.clone()))
                as Arc<dyn Tool>
        })
        .collect()
}

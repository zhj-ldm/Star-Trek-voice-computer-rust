use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

/// ToolSearch allows lazy discovery of tools by name or keyword.
pub struct ToolSearchTool {
    /// All available tools for searching.
    available_tools: Arc<RwLock<Vec<ToolInfo>>>,
}

#[derive(Clone, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl Default for ToolSearchTool {
    fn default() -> Self {
        Self {
            available_tools: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl ToolSearchTool {
    pub fn new(tools: Vec<ToolInfo>) -> Self {
        Self {
            available_tools: Arc::new(RwLock::new(tools)),
        }
    }

    pub async fn set_tools(&self, tools: Vec<ToolInfo>) {
        let mut available = self.available_tools.write().await;
        *available = tools;
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn description(&self) -> &str {
        "Search for available tools by name or keyword. Use 'select:Read,Edit' for exact names, or keywords for fuzzy search."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "query".to_string(),
                    json!({
                        "type": "string",
                        "description": "Search query. Use 'select:Tool1,Tool2' for exact matches, or keywords"
                    }),
                ),
                (
                    "max_results".to_string(),
                    json!({
                        "type": "number",
                        "description": "Maximum results to return (default 5)"
                    }),
                ),
            ]),
            required: vec!["query".to_string()],
            additional_properties: Some(false),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _context: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let query = input
            .get("query")
            .and_then(|q| q.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'query'".to_string()))?;

        let max_results = input
            .get("max_results")
            .and_then(|m| m.as_u64())
            .unwrap_or(5) as usize;

        let tools = self.available_tools.read().await;

        let matches: Vec<&ToolInfo> = if query.starts_with("select:") {
            let names: Vec<&str> = query[7..].split(',').map(|s| s.trim()).collect();
            tools
                .iter()
                .filter(|t| names.iter().any(|n| t.name.eq_ignore_ascii_case(n)))
                .collect()
        } else {
            let query_lower = query.to_lowercase();
            let keywords: Vec<&str> = query_lower.split_whitespace().collect();
            let mut scored: Vec<(&ToolInfo, usize)> = tools
                .iter()
                .filter_map(|t| {
                    let name_lower = t.name.to_lowercase();
                    let desc_lower = t.description.to_lowercase();
                    let score: usize = keywords
                        .iter()
                        .filter(|kw| name_lower.contains(*kw) || desc_lower.contains(*kw))
                        .count();
                    if score > 0 {
                        Some((t, score))
                    } else {
                        None
                    }
                })
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            scored.into_iter().map(|(t, _)| t).collect()
        };

        let matches: Vec<&ToolInfo> = matches.into_iter().take(max_results).collect();

        if matches.is_empty() {
            return Ok(ToolResult::text(format!(
                "No tools found matching: {}",
                query
            )));
        }

        let mut result = String::new();
        for tool in matches {
            result.push_str(&format!(
                "## {}\n{}\n\nInput Schema:\n```json\n{}\n```\n\n",
                tool.name,
                tool.description,
                serde_json::to_string_pretty(&tool.input_schema).unwrap_or_default()
            ));
        }

        Ok(ToolResult::text(result))
    }
}

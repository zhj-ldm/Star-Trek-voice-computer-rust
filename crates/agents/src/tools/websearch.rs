use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

/// Search result from a web search provider.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Type alias for a pluggable search function.
pub type SearchFn =
    Arc<dyn Fn(&str, usize) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> + Send + Sync>;

pub struct WebSearchTool {
    search_fn: Option<SearchFn>,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self { search_fn: None }
    }
}

impl WebSearchTool {
    pub fn new(search_fn: SearchFn) -> Self {
        Self {
            search_fn: Some(search_fn),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn description(&self) -> &str {
        "Searches the web for information. Requires a configured search provider."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "query".to_string(),
                    json!({
                        "type": "string",
                        "description": "The search query"
                    }),
                ),
                (
                    "max_results".to_string(),
                    json!({
                        "type": "number",
                        "description": "Maximum number of results (default 5)"
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
            .ok_or_else(|| ToolError::InvalidInput("Missing 'query' field".to_string()))?;

        let max_results = input
            .get("max_results")
            .and_then(|m| m.as_u64())
            .unwrap_or(5) as usize;

        match &self.search_fn {
            Some(search_fn) => {
                let results = (search_fn)(query, max_results)
                    .await
                    .map_err(|e| ToolError::ExecutionError(e))?;

                if results.is_empty() {
                    return Ok(ToolResult::text("No results found.".to_string()));
                }

                let formatted: Vec<String> = results
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        format!(
                            "{}. **{}**\n   URL: {}\n   {}",
                            i + 1,
                            r.title,
                            r.url,
                            r.snippet
                        )
                    })
                    .collect();

                Ok(ToolResult::text(formatted.join("\n\n")))
            }
            None => Ok(ToolResult::error(
                "Web search is not configured. Provide a search function when creating the agent."
                    .to_string(),
            )),
        }
    }
}

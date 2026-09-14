use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

const MAX_RESPONSE_SIZE: usize = 512 * 1024; // 512KB
const TIMEOUT_SECS: u64 = 30;

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetches content from a URL. Returns the response body with a 512KB size limit."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "url".to_string(),
                    json!({
                        "type": "string",
                        "description": "The URL to fetch"
                    }),
                ),
                (
                    "headers".to_string(),
                    json!({
                        "type": "object",
                        "description": "Optional HTTP headers"
                    }),
                ),
            ]),
            required: vec!["url".to_string()],
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
        let url = input
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'url' field".to_string()))?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build()
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

        let mut req = client.get(url);

        if let Some(headers) = input.get("headers").and_then(|h| h.as_object()) {
            for (key, value) in headers {
                if let Some(val) = value.as_str() {
                    req = req.header(key, val);
                }
            }
        }

        let response = req
            .send()
            .await
            .map_err(|e| ToolError::ExecutionError(format!("Fetch failed: {}", e)))?;

        let status = response.status().as_u16();
        if status >= 400 {
            return Ok(ToolResult::error(format!(
                "HTTP {} error fetching {}",
                status, url
            )));
        }

        let body = response
            .text()
            .await
            .map_err(|e| ToolError::ExecutionError(format!("Failed to read response: {}", e)))?;

        let mut text = body;
        if text.len() > MAX_RESPONSE_SIZE {
            text.truncate(MAX_RESPONSE_SIZE);
            text.push_str("\n... (response truncated at 512KB)");
        }

        Ok(ToolResult::text(text))
    }
}

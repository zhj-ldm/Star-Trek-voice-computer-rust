use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// MCP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport")]
pub enum McpServerConfig {
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    #[serde(rename = "sse")]
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// MCP tool definition received from server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
}

/// Result from calling an MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallResult {
    #[serde(default)]
    pub content: Vec<McpContentBlock>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// MCP server connection state.
#[derive(Debug, Clone)]
pub struct McpServerConnection {
    pub name: String,
    pub config: McpServerConfig,
    pub tools: Vec<McpToolDefinition>,
    pub connected: bool,
}

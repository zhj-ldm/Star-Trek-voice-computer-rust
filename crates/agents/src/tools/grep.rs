use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

const DEFAULT_HEAD_LIMIT: usize = 250;

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "A powerful search tool built on ripgrep. Supports full regex syntax, file type filtering, and multiple output modes."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "pattern".to_string(),
                    json!({
                        "type": "string",
                        "description": "The regex pattern to search for"
                    }),
                ),
                (
                    "path".to_string(),
                    json!({
                        "type": "string",
                        "description": "File or directory to search in"
                    }),
                ),
                (
                    "output_mode".to_string(),
                    json!({
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"],
                        "description": "Output mode (default: files_with_matches)"
                    }),
                ),
                (
                    "glob".to_string(),
                    json!({
                        "type": "string",
                        "description": "Glob pattern to filter files (e.g. \"*.rs\")"
                    }),
                ),
                (
                    "type".to_string(),
                    json!({
                        "type": "string",
                        "description": "File type to search (e.g. rust, js, py)"
                    }),
                ),
                (
                    "-i".to_string(),
                    json!({
                        "type": "boolean",
                        "description": "Case insensitive search"
                    }),
                ),
                (
                    "-n".to_string(),
                    json!({
                        "type": "boolean",
                        "description": "Show line numbers"
                    }),
                ),
                (
                    "-A".to_string(),
                    json!({
                        "type": "number",
                        "description": "Lines to show after each match"
                    }),
                ),
                (
                    "-B".to_string(),
                    json!({
                        "type": "number",
                        "description": "Lines to show before each match"
                    }),
                ),
                (
                    "-C".to_string(),
                    json!({
                        "type": "number",
                        "description": "Lines of context around each match"
                    }),
                ),
                (
                    "head_limit".to_string(),
                    json!({
                        "type": "number",
                        "description": "Limit output to first N entries (default 250)"
                    }),
                ),
                (
                    "multiline".to_string(),
                    json!({
                        "type": "boolean",
                        "description": "Enable multiline matching"
                    }),
                ),
            ]),
            required: vec!["pattern".to_string()],
            additional_properties: Some(false),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, context: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let pattern = input
            .get("pattern")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'pattern' field".to_string()))?;

        let search_path = input
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or(&context.working_dir);

        let output_mode = input
            .get("output_mode")
            .and_then(|m| m.as_str())
            .unwrap_or("files_with_matches");

        let head_limit = input
            .get("head_limit")
            .and_then(|h| h.as_u64())
            .unwrap_or(DEFAULT_HEAD_LIMIT as u64) as usize;

        // Try rg first, fall back to grep
        let result = run_ripgrep(pattern, search_path, output_mode, &input).await;

        match result {
            Ok(output) => {
                if output.is_empty() {
                    return Ok(ToolResult::text("No matches found.".to_string()));
                }

                let lines: Vec<&str> = output.lines().collect();
                let total = lines.len();
                let truncated = total > head_limit;
                let lines: Vec<&str> = lines.into_iter().take(head_limit).collect();
                let mut result = lines.join("\n");

                if truncated {
                    result.push_str(&format!(
                        "\n\n(showing {} of {} results)",
                        head_limit, total
                    ));
                }

                Ok(ToolResult::text(result))
            }
            Err(e) => {
                // Fall back to grep
                match run_grep(pattern, search_path, output_mode, &input).await {
                    Ok(output) => {
                        if output.is_empty() {
                            return Ok(ToolResult::text("No matches found.".to_string()));
                        }
                        Ok(ToolResult::text(output))
                    }
                    Err(_) => Ok(ToolResult::error(format!("Search failed: {}", e))),
                }
            }
        }
    }
}

async fn run_ripgrep(
    pattern: &str,
    path: &str,
    output_mode: &str,
    input: &Value,
) -> Result<String, String> {
    let mut args = vec!["--no-heading".to_string()];

    match output_mode {
        "files_with_matches" => args.push("-l".to_string()),
        "count" => args.push("-c".to_string()),
        "content" => {
            if input.get("-n").and_then(|n| n.as_bool()).unwrap_or(true) {
                args.push("-n".to_string());
            }
        }
        _ => args.push("-l".to_string()),
    }

    if input.get("-i").and_then(|i| i.as_bool()).unwrap_or(false) {
        args.push("-i".to_string());
    }

    if let Some(after) = input.get("-A").and_then(|a| a.as_u64()) {
        args.push(format!("-A{}", after));
    }
    if let Some(before) = input.get("-B").and_then(|b| b.as_u64()) {
        args.push(format!("-B{}", before));
    }
    if let Some(ctx) = input.get("-C").and_then(|c| c.as_u64()) {
        args.push(format!("-C{}", ctx));
    }

    if let Some(glob_pattern) = input.get("glob").and_then(|g| g.as_str()) {
        args.push("--glob".to_string());
        args.push(glob_pattern.to_string());
    }

    if let Some(file_type) = input.get("type").and_then(|t| t.as_str()) {
        args.push("--type".to_string());
        args.push(file_type.to_string());
    }

    if input
        .get("multiline")
        .and_then(|m| m.as_bool())
        .unwrap_or(false)
    {
        args.push("-U".to_string());
        args.push("--multiline-dotall".to_string());
    }

    args.push(pattern.to_string());
    args.push(path.to_string());

    let output = Command::new("rg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if output.status.success() || output.status.code() == Some(1) {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

async fn run_grep(
    pattern: &str,
    path: &str,
    output_mode: &str,
    input: &Value,
) -> Result<String, String> {
    let mut args = vec!["-r".to_string()];

    match output_mode {
        "files_with_matches" => args.push("-l".to_string()),
        "count" => args.push("-c".to_string()),
        "content" => {
            args.push("-n".to_string());
        }
        _ => args.push("-l".to_string()),
    }

    if input.get("-i").and_then(|i| i.as_bool()).unwrap_or(false) {
        args.push("-i".to_string());
    }

    args.push(pattern.to_string());
    args.push(path.to_string());

    let output = Command::new("grep")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| e.to_string())?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

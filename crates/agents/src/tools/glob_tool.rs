use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

const MAX_RESULTS: usize = 100;

/// Directories to skip during glob matching.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".next",
    "__pycache__",
    ".mypy_cache",
    "target",
    "dist",
    "build",
];

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Fast file pattern matching tool. Supports glob patterns like \"**/*.rs\" or \"src/**/*.ts\". Returns matching file paths sorted by modification time."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "pattern".to_string(),
                    json!({
                        "type": "string",
                        "description": "The glob pattern to match files against"
                    }),
                ),
                (
                    "path".to_string(),
                    json!({
                        "type": "string",
                        "description": "The directory to search in (defaults to working directory)"
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

        let full_pattern = if pattern.starts_with('/') {
            pattern.to_string()
        } else {
            format!("{}/{}", search_path, pattern)
        };

        let entries = glob::glob(&full_pattern)
            .map_err(|e| ToolError::ExecutionError(format!("Invalid glob pattern: {}", e)))?;

        let mut files: Vec<(String, std::time::SystemTime)> = Vec::new();

        for entry in entries {
            if let Ok(path) = entry {
                // Skip directories in SKIP_DIRS
                let should_skip = path.components().any(|c| {
                    if let std::path::Component::Normal(os_str) = c {
                        SKIP_DIRS.contains(&os_str.to_str().unwrap_or(""))
                    } else {
                        false
                    }
                });

                if should_skip {
                    continue;
                }

                if path.is_file() {
                    let modified = path
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    files.push((path.to_string_lossy().to_string(), modified));
                }
            }
        }

        // Sort by modification time (newest first)
        files.sort_by(|a, b| b.1.cmp(&a.1));

        let total = files.len();
        let truncated = total > MAX_RESULTS;
        let files: Vec<String> = files.into_iter().take(MAX_RESULTS).map(|(p, _)| p).collect();

        if files.is_empty() {
            return Ok(ToolResult::text(format!(
                "No files found matching pattern: {}",
                pattern
            )));
        }

        let mut result = files.join("\n");
        if truncated {
            result.push_str(&format!(
                "\n\n(showing {} of {} matches)",
                MAX_RESULTS, total
            ));
        }

        Ok(ToolResult::text(result))
    }
}

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

const DEFAULT_LINE_LIMIT: usize = 2000;
const MAX_CONTENT_SIZE: usize = 100_000;

pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Reads a file from the local filesystem. Returns content with line numbers."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "file_path".to_string(),
                    json!({
                        "type": "string",
                        "description": "The absolute path to the file to read"
                    }),
                ),
                (
                    "offset".to_string(),
                    json!({
                        "type": "integer",
                        "description": "The line number to start reading from (0-based)"
                    }),
                ),
                (
                    "limit".to_string(),
                    json!({
                        "type": "integer",
                        "description": "The number of lines to read"
                    }),
                ),
            ]),
            required: vec!["file_path".to_string()],
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
        let file_path = input
            .get("file_path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'file_path' field".to_string()))?;

        let path = resolve_path(file_path, &context.working_dir);

        // Check if file is a device file
        if file_path.starts_with("/dev/") || file_path.starts_with("/proc/") {
            return Ok(ToolResult::error(
                "Cannot read device or proc files".to_string(),
            ));
        }

        if !path.exists() {
            return Ok(ToolResult::error(format!(
                "File not found: {}",
                path.display()
            )));
        }

        if path.is_dir() {
            return Ok(ToolResult::error(format!(
                "{} is a directory, not a file. Use Bash with 'ls' to list directory contents.",
                path.display()
            )));
        }

        // Check for image files
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg") {
            return read_image(&path, &ext).await;
        }

        // Read text file
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::IoError(e))?;

        // Track file state for staleness detection
        {
            let hash = simple_hash(&content);
            let mut state = context.read_file_state.write().await;
            state.insert(path.to_string_lossy().to_string(), hash);
        }

        let offset = input.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
        let limit = input
            .get("limit")
            .and_then(|l| l.as_u64())
            .unwrap_or(DEFAULT_LINE_LIMIT as u64) as usize;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let start = offset.min(total_lines);
        let end = (start + limit).min(total_lines);
        let selected_lines = &lines[start..end];

        let mut result = String::new();
        for (i, line) in selected_lines.iter().enumerate() {
            let line_num = start + i + 1;
            result.push_str(&format!("{}\t{}\n", line_num, line));
        }

        if result.len() > MAX_CONTENT_SIZE {
            result.truncate(MAX_CONTENT_SIZE);
            result.push_str("\n... (content truncated)");
        }

        if end < total_lines {
            result.push_str(&format!(
                "\n(showing lines {}-{} of {})",
                start + 1,
                end,
                total_lines
            ));
        }

        Ok(ToolResult::text(result))
    }
}

async fn read_image(path: &Path, ext: &str) -> Result<ToolResult, ToolError> {
    let data = tokio::fs::read(path).await.map_err(ToolError::IoError)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    let media_type = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    };
    Ok(ToolResult::image(encoded, media_type.to_string()))
}

fn resolve_path(file_path: &str, working_dir: &str) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(working_dir).join(file_path)
    }
}

fn simple_hash(content: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

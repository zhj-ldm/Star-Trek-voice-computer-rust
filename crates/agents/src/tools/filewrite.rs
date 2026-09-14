use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

pub struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Writes a file to the local filesystem. Will overwrite existing files. Use Read tool first for existing files."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "file_path".to_string(),
                    json!({
                        "type": "string",
                        "description": "The absolute path to the file to write"
                    }),
                ),
                (
                    "content".to_string(),
                    json!({
                        "type": "string",
                        "description": "The content to write to the file"
                    }),
                ),
            ]),
            required: vec!["file_path".to_string(), "content".to_string()],
            additional_properties: Some(false),
        }
    }

    async fn call(&self, input: Value, context: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let file_path = input
            .get("file_path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'file_path' field".to_string()))?;

        let content = input
            .get("content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'content' field".to_string()))?;

        let path = if Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            Path::new(&context.working_dir).join(file_path)
        };

        let is_new = !path.exists();

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(ToolError::IoError)?;
        }

        // Check staleness for existing files
        if !is_new {
            let current_content = tokio::fs::read_to_string(&path).await.ok();
            if let Some(current) = &current_content {
                let current_hash = simple_hash(current);
                let state = context.read_file_state.read().await;
                if let Some(saved_hash) = state.get(&path.to_string_lossy().to_string()) {
                    if *saved_hash != current_hash {
                        return Ok(ToolResult::error(
                            "File has been modified since last read. Please Read the file again before writing."
                                .to_string(),
                        ));
                    }
                }
            }

            // Generate diff for existing files
            if let Some(old_content) = current_content {
                let diff = super::diff::unified_diff(&old_content, content, file_path);
                tokio::fs::write(&path, content)
                    .await
                    .map_err(ToolError::IoError)?;

                // Update file state
                let hash = simple_hash(content);
                let mut state = context.read_file_state.write().await;
                state.insert(path.to_string_lossy().to_string(), hash);

                return Ok(ToolResult::text(format!(
                    "Updated file: {}\n\n{}",
                    path.display(),
                    diff
                )));
            }
        }

        tokio::fs::write(&path, content)
            .await
            .map_err(ToolError::IoError)?;

        // Update file state
        let hash = simple_hash(content);
        let mut state = context.read_file_state.write().await;
        state.insert(path.to_string_lossy().to_string(), hash);

        let action = if is_new { "Created" } else { "Updated" };
        Ok(ToolResult::text(format!(
            "{} file: {} ({} bytes)",
            action,
            path.display(),
            content.len()
        )))
    }
}

fn simple_hash(content: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

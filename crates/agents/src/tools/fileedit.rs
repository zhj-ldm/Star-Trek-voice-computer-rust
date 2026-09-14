use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

pub struct FileEditTool;

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Performs exact string replacements in files. The old_string must be unique in the file unless replace_all is true."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "file_path".to_string(),
                    json!({
                        "type": "string",
                        "description": "The absolute path to the file to modify"
                    }),
                ),
                (
                    "old_string".to_string(),
                    json!({
                        "type": "string",
                        "description": "The text to replace"
                    }),
                ),
                (
                    "new_string".to_string(),
                    json!({
                        "type": "string",
                        "description": "The replacement text"
                    }),
                ),
                (
                    "replace_all".to_string(),
                    json!({
                        "type": "boolean",
                        "description": "Replace all occurrences (default false)",
                        "default": false
                    }),
                ),
            ]),
            required: vec![
                "file_path".to_string(),
                "old_string".to_string(),
                "new_string".to_string(),
            ],
            additional_properties: Some(false),
        }
    }

    async fn call(&self, input: Value, context: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let file_path = input
            .get("file_path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'file_path' field".to_string()))?;

        let old_string = input
            .get("old_string")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'old_string' field".to_string()))?;

        let new_string = input
            .get("new_string")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'new_string' field".to_string()))?;

        let replace_all = input
            .get("replace_all")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);

        if old_string == new_string {
            return Ok(ToolResult::error(
                "old_string and new_string must be different".to_string(),
            ));
        }

        let path = if Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            Path::new(&context.working_dir).join(file_path)
        };

        if !path.exists() {
            return Ok(ToolResult::error(format!(
                "File not found: {}",
                path.display()
            )));
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(ToolError::IoError)?;

        // Check staleness
        {
            let current_hash = simple_hash(&content);
            let state = context.read_file_state.read().await;
            if let Some(saved_hash) = state.get(&path.to_string_lossy().to_string()) {
                if *saved_hash != current_hash {
                    return Ok(ToolResult::error(
                        "File has been modified since last read. Please Read the file again before editing."
                            .to_string(),
                    ));
                }
            }
        }

        // Count occurrences
        let count = content.matches(old_string).count();

        if count == 0 {
            return Ok(ToolResult::error(format!(
                "old_string not found in {}. Make sure the string matches exactly, including whitespace and indentation.",
                path.display()
            )));
        }

        if count > 1 && !replace_all {
            return Ok(ToolResult::error(format!(
                "old_string found {} times in {}. Either provide more context to make it unique, or set replace_all to true.",
                count,
                path.display()
            )));
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        let diff = super::diff::unified_diff(&content, &new_content, file_path);

        tokio::fs::write(&path, &new_content)
            .await
            .map_err(ToolError::IoError)?;

        // Update file state
        let hash = simple_hash(&new_content);
        let mut state = context.read_file_state.write().await;
        state.insert(path.to_string_lossy().to_string(), hash);

        let replacements = if replace_all {
            format!(" ({} replacements)", count)
        } else {
            String::new()
        };

        Ok(ToolResult::text(format!(
            "Edited file: {}{}\n\n{}",
            path.display(),
            replacements,
            diff
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

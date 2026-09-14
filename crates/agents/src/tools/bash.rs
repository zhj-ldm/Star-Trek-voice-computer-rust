use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command;

use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_SIZE: usize = 100_000;

/// Destructive command patterns that should be flagged.
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -rf .",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "chmod 777",
    "chmod -R 777",
    "> /dev/sda",
    "mkfs.",
    "dd if=",
    ":(){ :|:& };:",
];

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Executes a given bash command and returns its output. Use for system commands and terminal operations that require shell execution."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "command".to_string(),
                    json!({
                        "type": "string",
                        "description": "The command to execute"
                    }),
                ),
                (
                    "timeout".to_string(),
                    json!({
                        "type": "number",
                        "description": "Optional timeout in milliseconds (max 600000)"
                    }),
                ),
                (
                    "description".to_string(),
                    json!({
                        "type": "string",
                        "description": "Clear description of what this command does"
                    }),
                ),
            ]),
            required: vec!["command".to_string()],
            additional_properties: Some(false),
        }
    }

    fn is_read_only(&self, input: &Value) -> bool {
        let command = input
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        // Check if command starts with a read-only command
        let cmd_trimmed = command.trim();
        let first_cmd = cmd_trimmed.split_whitespace().next().unwrap_or("");
        let single_word_reads = [
            "ls", "cat", "head", "tail", "find", "grep", "rg", "wc", "pwd", "echo", "which",
            "type", "file", "stat", "du", "df",
        ];
        let prefix_reads = [
            "git status", "git log", "git diff", "git show", "git branch",
            "cargo check", "cargo test --no-run", "rustc --version",
        ];

        single_word_reads.contains(&first_cmd) || prefix_reads.iter().any(|p| cmd_trimmed.starts_with(p))
    }

    async fn call(&self, input: Value, context: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let command = input
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'command' field".to_string()))?;

        // Security check
        if let Some(warning) = check_destructive(command) {
            return Ok(ToolResult::error(format!(
                "Potentially destructive command detected: {}. Proceed with caution.",
                warning
            )));
        }

        let timeout_ms = input
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            run_command(command, &context.working_dir),
        )
        .await;

        match output {
            Ok(Ok((stdout, stderr, exit_code))) => {
                let mut result = String::new();

                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str("STDERR:\n");
                    result.push_str(&stderr);
                }

                if result.len() > MAX_OUTPUT_SIZE {
                    result.truncate(MAX_OUTPUT_SIZE);
                    result.push_str("\n... (output truncated)");
                }

                if exit_code != 0 {
                    result.push_str(&format!("\n\nExit code: {}", exit_code));
                }

                if result.is_empty() {
                    result = "(no output)".to_string();
                }

                Ok(if exit_code != 0 {
                    ToolResult::error(result)
                } else {
                    ToolResult::text(result)
                })
            }
            Ok(Err(e)) => Ok(ToolResult::error(format!("Command failed: {}", e))),
            Err(_) => Ok(ToolResult::error(format!(
                "Command timed out after {}ms",
                timeout_ms
            ))),
        }
    }
}

async fn run_command(
    command: &str,
    working_dir: &str,
) -> Result<(String, String, i32), std::io::Error> {
    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    Ok((stdout, stderr, exit_code))
}

fn check_destructive(command: &str) -> Option<&'static str> {
    for pattern in DESTRUCTIVE_PATTERNS {
        if command.contains(pattern) {
            return Some(pattern);
        }
    }
    None
}

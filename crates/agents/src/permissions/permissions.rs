use crate::types::PermissionMode;
use serde_json::Value;

/// A permission rule that matches tool invocations.
#[derive(Debug, Clone)]
pub struct Rule {
    pub tool_name: String,
    pub pattern: Option<String>,
}

/// Permission configuration.
#[derive(Debug, Clone)]
pub struct PermissionConfig {
    pub mode: PermissionMode,
    pub allow_rules: Vec<Rule>,
    pub deny_rules: Vec<Rule>,
    pub allowed_tools: Option<Vec<String>>,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            mode: PermissionMode::BypassPermissions,
            allow_rules: Vec::new(),
            deny_rules: Vec::new(),
            allowed_tools: None,
        }
    }
}

/// Result of a permission check.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResult {
    Allow,
    Deny(String),
}

/// Check if a tool invocation is allowed.
pub fn check_permission(
    config: &PermissionConfig,
    tool_name: &str,
    input: &Value,
) -> PermissionResult {
    // Bypass mode allows everything
    if config.mode == PermissionMode::BypassPermissions {
        return PermissionResult::Allow;
    }

    // Check allowed tools whitelist
    if let Some(allowed) = &config.allowed_tools {
        if !allowed.iter().any(|a| a == tool_name) {
            return PermissionResult::Deny(format!("Tool '{}' is not in allowed list", tool_name));
        }
    }

    // Check deny rules first
    for rule in &config.deny_rules {
        if matches_rule(&rule.tool_name, tool_name) {
            if let Some(pattern) = &rule.pattern {
                if matches_input_pattern(pattern, input) {
                    return PermissionResult::Deny(format!(
                        "Tool '{}' denied by rule",
                        tool_name
                    ));
                }
            } else {
                return PermissionResult::Deny(format!(
                    "Tool '{}' denied by rule",
                    tool_name
                ));
            }
        }
    }

    // Check allow rules
    for rule in &config.allow_rules {
        if matches_rule(&rule.tool_name, tool_name) {
            return PermissionResult::Allow;
        }
    }

    // Default behavior based on mode
    match config.mode {
        PermissionMode::DontAsk | PermissionMode::Auto => PermissionResult::Allow,
        PermissionMode::AcceptEdits => {
            // Allow read-only tools and file edits
            let read_only_tools = [
                "Read", "Glob", "Grep", "WebFetch", "WebSearch", "TaskList", "TaskGet",
                "ToolSearch",
            ];
            if read_only_tools.contains(&tool_name) {
                PermissionResult::Allow
            } else {
                PermissionResult::Allow // acceptEdits mode allows edits
            }
        }
        PermissionMode::Plan => PermissionResult::Deny(format!(
            "Tool '{}' requires approval in plan mode",
            tool_name
        )),
        _ => PermissionResult::Allow,
    }
}

fn matches_rule(rule_name: &str, tool_name: &str) -> bool {
    if rule_name == "*" {
        return true;
    }
    if rule_name.ends_with('*') {
        let prefix = &rule_name[..rule_name.len() - 1];
        return tool_name.starts_with(prefix);
    }
    rule_name == tool_name
}

fn matches_input_pattern(pattern: &str, input: &Value) -> bool {
    let input_str = input.to_string();
    input_str.contains(pattern)
}

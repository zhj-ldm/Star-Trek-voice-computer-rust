use crate::types::{CacheControl, SystemBlock};
use std::process::Command;
use tokio::sync::OnceCell;

static GIT_STATUS_CACHE: OnceCell<String> = OnceCell::const_new();

/// Get git status for the given working directory.
pub fn get_git_status(cwd: &str) -> String {
    // Try to get cached value first synchronously
    if let Some(cached) = GIT_STATUS_CACHE.get() {
        return cached.clone();
    }

    let status = compute_git_status(cwd);
    // Best effort to cache
    let _ = GIT_STATUS_CACHE.set(status.clone());
    status
}

fn compute_git_status(cwd: &str) -> String {
    let mut result = String::new();

    // Get branch
    if let Ok(output) = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
    {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !branch.is_empty() {
            result.push_str(&format!("Current branch: {}\n", branch));
        }
    }

    // Get main branch
    let main_branch = detect_main_branch(cwd);
    result.push_str(&format!(
        "\nMain branch: {}\n",
        main_branch
    ));

    // Get git user
    if let Ok(output) = Command::new("git")
        .args(["config", "user.name"])
        .current_dir(cwd)
        .output()
    {
        let user = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !user.is_empty() {
            result.push_str(&format!("\nGit user: {}\n", user));
        }
    }

    // Get status
    if let Ok(output) = Command::new("git")
        .args(["status", "--short"])
        .current_dir(cwd)
        .output()
    {
        let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if status.is_empty() {
            result.push_str("\nStatus:\n(clean)\n");
        } else {
            result.push_str(&format!("\nStatus:\n{}\n", status));
        }
    }

    // Get recent commits
    if let Ok(output) = Command::new("git")
        .args(["log", "--oneline", "-5"])
        .current_dir(cwd)
        .output()
    {
        let log = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !log.is_empty() {
            result.push_str(&format!("\nRecent commits:\n{}\n", log));
        }
    }

    result
}

fn detect_main_branch(cwd: &str) -> String {
    // Check for main first, then master
    for branch in &["main", "master"] {
        if let Ok(output) = Command::new("git")
            .args(["rev-parse", "--verify", branch])
            .current_dir(cwd)
            .output()
        {
            if output.status.success() {
                return branch.to_string();
            }
        }
    }
    "main".to_string()
}

/// Build system prompt blocks with context injection.
pub fn build_system_blocks(
    cwd: &str,
    custom_system_prompt: Option<&str>,
    append_system_prompt: Option<&str>,
) -> Vec<SystemBlock> {
    let mut blocks = Vec::new();

    // Main system prompt with cache control
    let system_prompt = if let Some(custom) = custom_system_prompt {
        custom.to_string()
    } else {
        default_system_prompt()
    };

    blocks.push(SystemBlock {
        block_type: "text".to_string(),
        text: system_prompt,
        cache_control: Some(CacheControl::ephemeral()),
    });

    // Git status context
    let git_status = get_git_status(cwd);
    if !git_status.is_empty() {
        blocks.push(SystemBlock {
            block_type: "text".to_string(),
            text: format!("gitStatus: {}", git_status),
            cache_control: None,
        });
    }

    // User context (date, project files)
    let user_context = get_user_context(cwd);
    if !user_context.is_empty() {
        blocks.push(SystemBlock {
            block_type: "text".to_string(),
            text: user_context,
            cache_control: Some(CacheControl::ephemeral()),
        });
    }

    // Append system prompt
    if let Some(append) = append_system_prompt {
        blocks.push(SystemBlock {
            block_type: "text".to_string(),
            text: append.to_string(),
            cache_control: None,
        });
    }

    blocks
}

fn default_system_prompt() -> String {
    "You are a helpful AI assistant with access to tools for software engineering tasks. \
     Use the available tools to help the user accomplish their goals. \
     Be concise and direct in your responses."
        .to_string()
}

fn get_user_context(cwd: &str) -> String {
    let mut context = String::new();

    // Current date
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    context.push_str(&format!("Current date: {}\n", date));

    // Check for project context files
    for filename in &["AGENT.md", "CLAUDE.md", ".agent/AGENT.md"] {
        let path = std::path::Path::new(cwd).join(filename);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                context.push_str(&format!(
                    "\n# Project context from {}\n{}\n",
                    filename, content
                ));
            }
        }
    }

    context
}

/// Clear the cached git status.
pub fn clear_context_cache() {
    // OnceCell doesn't support reset, but this is a session-level cache
    // that typically doesn't need clearing during normal operation
}

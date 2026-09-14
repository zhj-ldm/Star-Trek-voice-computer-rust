use open_agent_sdk::tools::ToolRegistry;
use open_agent_sdk::types::{Tool, ToolUseContext};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

fn create_test_context(dir: &str) -> ToolUseContext {
    ToolUseContext {
        working_dir: dir.to_string(),
        abort_signal: tokio_util::sync::CancellationToken::new(),
        read_file_state: Arc::new(RwLock::new(std::collections::HashMap::new())),
    }
}

// --- Registry Tests ---

#[test]
fn test_default_registry() {
    let registry = ToolRegistry::default_registry();
    assert!(registry.len() > 0);

    // Check core tools exist
    assert!(registry.get("Bash").is_some());
    assert!(registry.get("Read").is_some());
    assert!(registry.get("Write").is_some());
    assert!(registry.get("Edit").is_some());
    assert!(registry.get("Glob").is_some());
    assert!(registry.get("Grep").is_some());
    assert!(registry.get("WebFetch").is_some());
    assert!(registry.get("WebSearch").is_some());
    assert!(registry.get("AskUserQuestion").is_some());
    assert!(registry.get("TaskCreate").is_some());
    assert!(registry.get("TaskGet").is_some());
    assert!(registry.get("TaskList").is_some());
    assert!(registry.get("TaskUpdate").is_some());
    assert!(registry.get("ToolSearch").is_some());
}

#[test]
fn test_registry_register_custom() {
    use async_trait::async_trait;
    use open_agent_sdk::types::{ToolError, ToolInputSchema, ToolResult};
    use serde_json::Value;

    struct CustomTool;

    #[async_trait]
    impl Tool for CustomTool {
        fn name(&self) -> &str {
            "CustomTool"
        }
        fn description(&self) -> &str {
            "A test tool"
        }
        fn input_schema(&self) -> ToolInputSchema {
            ToolInputSchema::default()
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: &ToolUseContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("custom result"))
        }
    }

    let mut registry = ToolRegistry::new();
    assert!(registry.is_empty());

    registry.register(Arc::new(CustomTool));
    assert_eq!(registry.len(), 1);
    assert!(registry.get("CustomTool").is_some());
}

#[test]
fn test_registry_filter() {
    let registry = ToolRegistry::default_registry();
    let read_only = registry.filter(|t| t.is_read_only(&json!({})));
    assert!(read_only.len() > 0);

    // Read, Glob, Grep should be read-only
    let names: Vec<&str> = read_only.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"Read"));
    assert!(names.contains(&"Glob"));
    assert!(names.contains(&"Grep"));
}

#[test]
fn test_registry_retain() {
    let mut registry = ToolRegistry::default_registry();
    let initial_count = registry.len();

    registry.retain(&["Read", "Glob", "Grep"]);
    assert_eq!(registry.len(), 3);
    assert!(registry.len() < initial_count);

    assert!(registry.get("Read").is_some());
    assert!(registry.get("Bash").is_none());
}

#[test]
fn test_registry_remove() {
    let mut registry = ToolRegistry::default_registry();
    assert!(registry.get("Bash").is_some());

    registry.remove(&["Bash"]);
    assert!(registry.get("Bash").is_none());
}

// --- Bash Tool Tests ---

#[tokio::test]
async fn test_bash_echo() {
    let registry = ToolRegistry::default_registry();
    let bash = registry.get("Bash").unwrap();
    let ctx = create_test_context("/tmp");

    let result = bash
        .call(json!({"command": "echo 'hello world'"}), &ctx)
        .await
        .unwrap();

    assert!(!result.is_error);
    assert!(result.get_text().contains("hello world"));
}

#[tokio::test]
async fn test_bash_exit_code() {
    let registry = ToolRegistry::default_registry();
    let bash = registry.get("Bash").unwrap();
    let ctx = create_test_context("/tmp");

    let result = bash
        .call(json!({"command": "false"}), &ctx)
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.get_text().contains("Exit code:"));
}

#[tokio::test]
async fn test_bash_destructive_detection() {
    let registry = ToolRegistry::default_registry();
    let bash = registry.get("Bash").unwrap();
    let ctx = create_test_context("/tmp");

    let result = bash
        .call(json!({"command": "rm -rf /"}), &ctx)
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.get_text().contains("destructive"));
}

#[tokio::test]
async fn test_bash_is_read_only() {
    let registry = ToolRegistry::default_registry();
    let bash = registry.get("Bash").unwrap();

    assert!(bash.is_read_only(&json!({"command": "ls -la"})));
    assert!(bash.is_read_only(&json!({"command": "git status"})));
    assert!(!bash.is_read_only(&json!({"command": "rm file.txt"})));
    assert!(!bash.is_read_only(&json!({"command": "cargo build"})));
}

// --- File Read Tool Tests ---

#[tokio::test]
async fn test_read_file() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.txt");
    std::fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

    let registry = ToolRegistry::default_registry();
    let read = registry.get("Read").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    let result = read
        .call(
            json!({"file_path": file_path.to_str().unwrap()}),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    let text = result.get_text();
    assert!(text.contains("line 1"));
    assert!(text.contains("line 2"));
    assert!(text.contains("line 3"));
    // Should have line numbers
    assert!(text.contains("1\t"));
}

#[tokio::test]
async fn test_read_file_not_found() {
    let registry = ToolRegistry::default_registry();
    let read = registry.get("Read").unwrap();
    let ctx = create_test_context("/tmp");

    let result = read
        .call(json!({"file_path": "/tmp/nonexistent_file_12345.txt"}), &ctx)
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.get_text().contains("not found"));
}

#[tokio::test]
async fn test_read_file_with_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.txt");
    let content: String = (1..=100).map(|i| format!("line {}\n", i)).collect();
    std::fs::write(&file_path, &content).unwrap();

    let registry = ToolRegistry::default_registry();
    let read = registry.get("Read").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    let result = read
        .call(
            json!({
                "file_path": file_path.to_str().unwrap(),
                "offset": 10,
                "limit": 5
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    let text = result.get_text();
    assert!(text.contains("line 11"));
    assert!(text.contains("line 15"));
    assert!(!text.contains("line 16"));
}

#[tokio::test]
async fn test_read_directory_error() {
    let registry = ToolRegistry::default_registry();
    let read = registry.get("Read").unwrap();
    let ctx = create_test_context("/tmp");

    let result = read
        .call(json!({"file_path": "/tmp"}), &ctx)
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.get_text().contains("directory"));
}

#[tokio::test]
async fn test_read_is_read_only() {
    let registry = ToolRegistry::default_registry();
    let read = registry.get("Read").unwrap();
    assert!(read.is_read_only(&json!({})));
    assert!(read.is_concurrency_safe(&json!({})));
}

// --- File Write Tool Tests ---

#[tokio::test]
async fn test_write_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("new_file.txt");

    let registry = ToolRegistry::default_registry();
    let write = registry.get("Write").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    let result = write
        .call(
            json!({
                "file_path": file_path.to_str().unwrap(),
                "content": "hello world"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    assert!(result.get_text().contains("Created"));
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "hello world");
}

#[tokio::test]
async fn test_write_creates_directories() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("a/b/c/file.txt");

    let registry = ToolRegistry::default_registry();
    let write = registry.get("Write").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    let result = write
        .call(
            json!({
                "file_path": file_path.to_str().unwrap(),
                "content": "nested"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    assert!(file_path.exists());
}

// --- File Edit Tool Tests ---

#[tokio::test]
async fn test_edit_file() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("edit_test.txt");
    std::fs::write(&file_path, "hello world").unwrap();

    let registry = ToolRegistry::default_registry();
    let edit = registry.get("Edit").unwrap();
    let read = registry.get("Read").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    // Must read first for staleness tracking
    read.call(
        json!({"file_path": file_path.to_str().unwrap()}),
        &ctx,
    )
    .await
    .unwrap();

    let result = edit
        .call(
            json!({
                "file_path": file_path.to_str().unwrap(),
                "old_string": "hello",
                "new_string": "goodbye"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "goodbye world"
    );
}

#[tokio::test]
async fn test_edit_string_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("edit_test2.txt");
    std::fs::write(&file_path, "hello world").unwrap();

    let registry = ToolRegistry::default_registry();
    let edit = registry.get("Edit").unwrap();
    let read = registry.get("Read").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    read.call(
        json!({"file_path": file_path.to_str().unwrap()}),
        &ctx,
    )
    .await
    .unwrap();

    let result = edit
        .call(
            json!({
                "file_path": file_path.to_str().unwrap(),
                "old_string": "xyz",
                "new_string": "abc"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.get_text().contains("not found"));
}

#[tokio::test]
async fn test_edit_multiple_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("edit_multi.txt");
    std::fs::write(&file_path, "foo bar foo baz foo").unwrap();

    let registry = ToolRegistry::default_registry();
    let edit = registry.get("Edit").unwrap();
    let read = registry.get("Read").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    read.call(
        json!({"file_path": file_path.to_str().unwrap()}),
        &ctx,
    )
    .await
    .unwrap();

    // Without replace_all, should fail for multiple occurrences
    let result = edit
        .call(
            json!({
                "file_path": file_path.to_str().unwrap(),
                "old_string": "foo",
                "new_string": "qux"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.get_text().contains("3 times"));
}

#[tokio::test]
async fn test_edit_replace_all() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("edit_all.txt");
    std::fs::write(&file_path, "foo bar foo baz foo").unwrap();

    let registry = ToolRegistry::default_registry();
    let edit = registry.get("Edit").unwrap();
    let read = registry.get("Read").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    read.call(
        json!({"file_path": file_path.to_str().unwrap()}),
        &ctx,
    )
    .await
    .unwrap();

    let result = edit
        .call(
            json!({
                "file_path": file_path.to_str().unwrap(),
                "old_string": "foo",
                "new_string": "qux",
                "replace_all": true
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "qux bar qux baz qux"
    );
}

#[tokio::test]
async fn test_edit_same_string_error() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("edit_same.txt");
    std::fs::write(&file_path, "hello world").unwrap();

    let registry = ToolRegistry::default_registry();
    let edit = registry.get("Edit").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    let result = edit
        .call(
            json!({
                "file_path": file_path.to_str().unwrap(),
                "old_string": "hello",
                "new_string": "hello"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.get_text().contains("different"));
}

// --- Glob Tool Tests ---

#[tokio::test]
async fn test_glob_find_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "").unwrap();
    std::fs::write(dir.path().join("b.rs"), "").unwrap();
    std::fs::write(dir.path().join("c.txt"), "").unwrap();

    let registry = ToolRegistry::default_registry();
    let glob = registry.get("Glob").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    let result = glob
        .call(json!({"pattern": "*.rs"}), &ctx)
        .await
        .unwrap();

    assert!(!result.is_error);
    let text = result.get_text();
    assert!(text.contains("a.rs"));
    assert!(text.contains("b.rs"));
    assert!(!text.contains("c.txt"));
}

#[tokio::test]
async fn test_glob_no_matches() {
    let dir = tempfile::tempdir().unwrap();

    let registry = ToolRegistry::default_registry();
    let glob = registry.get("Glob").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    let result = glob
        .call(json!({"pattern": "*.xyz"}), &ctx)
        .await
        .unwrap();

    assert!(!result.is_error);
    assert!(result.get_text().contains("No files found"));
}

// --- Grep Tool Tests ---

#[tokio::test]
async fn test_grep_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "hello world\nfoo bar\nhello again\n").unwrap();

    let registry = ToolRegistry::default_registry();
    let grep = registry.get("Grep").unwrap();
    let ctx = create_test_context(dir.path().to_str().unwrap());

    let result = grep
        .call(
            json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    let text = result.get_text();
    assert!(text.contains("hello world") || text.contains("hello"));
}

// --- Task Tool Tests ---

#[tokio::test]
async fn test_task_lifecycle() {
    let registry = ToolRegistry::default_registry();
    let ctx = create_test_context("/tmp");

    // Create a task
    let create = registry.get("TaskCreate").unwrap();
    let result = create
        .call(
            json!({
                "subject": "Test task",
                "description": "A test task"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    assert!(result.get_text().contains("Created task"));

    // List tasks
    let list = registry.get("TaskList").unwrap();
    let result = list.call(json!({}), &ctx).await.unwrap();
    assert!(!result.is_error);
    assert!(result.get_text().contains("Test task"));

    // Get task
    let get = registry.get("TaskGet").unwrap();
    let result = get.call(json!({"id": "task_1"}), &ctx).await.unwrap();
    assert!(!result.is_error);
    assert!(result.get_text().contains("Test task"));

    // Update task
    let update = registry.get("TaskUpdate").unwrap();
    let result = update
        .call(json!({"id": "task_1", "status": "completed"}), &ctx)
        .await
        .unwrap();
    assert!(!result.is_error);

    // Verify update
    let result = get.call(json!({"id": "task_1"}), &ctx).await.unwrap();
    assert!(result.get_text().to_lowercase().contains("completed"));
}

#[tokio::test]
async fn test_task_not_found() {
    let registry = ToolRegistry::default_registry();
    let ctx = create_test_context("/tmp");

    let get = registry.get("TaskGet").unwrap();
    let result = get
        .call(json!({"id": "nonexistent"}), &ctx)
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(result.get_text().contains("not found"));
}

// --- WebFetch Tool Tests ---

#[tokio::test]
async fn test_webfetch_missing_url() {
    let registry = ToolRegistry::default_registry();
    let webfetch = registry.get("WebFetch").unwrap();
    let ctx = create_test_context("/tmp");

    let result = webfetch.call(json!({}), &ctx).await;
    assert!(result.is_err());
}

// --- Diff Tests ---

#[test]
fn test_unified_diff() {
    let old = "line 1\nline 2\nline 3\n";
    let new = "line 1\nline 2 modified\nline 3\n";
    let diff = open_agent_sdk::tools::diff::unified_diff(old, new, "test.txt");

    assert!(diff.contains("--- a/test.txt"));
    assert!(diff.contains("+++ b/test.txt"));
    assert!(diff.contains("-line 2"));
    assert!(diff.contains("+line 2 modified"));
}

#[test]
fn test_count_changes() {
    let old = "a\nb\nc\n";
    let new = "a\nx\nc\nd\n";
    let (added, removed) = open_agent_sdk::tools::diff::count_changes(old, new);
    assert!(added >= 1);
    assert!(removed >= 1);
}

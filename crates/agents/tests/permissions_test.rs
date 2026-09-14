use open_agent_sdk::permissions::*;
use open_agent_sdk::types::PermissionMode;
use serde_json::json;

#[test]
fn test_bypass_mode_allows_everything() {
    let config = PermissionConfig {
        mode: PermissionMode::BypassPermissions,
        ..Default::default()
    };

    assert_eq!(
        check_permission(&config, "Bash", &json!({"command": "rm -rf /"})),
        PermissionResult::Allow
    );
    assert_eq!(
        check_permission(&config, "Write", &json!({})),
        PermissionResult::Allow
    );
}

#[test]
fn test_deny_rules() {
    let config = PermissionConfig {
        mode: PermissionMode::Default,
        deny_rules: vec![Rule {
            tool_name: "Bash".to_string(),
            pattern: None,
        }],
        ..Default::default()
    };

    assert_eq!(
        check_permission(&config, "Bash", &json!({})),
        PermissionResult::Deny("Tool 'Bash' denied by rule".to_string())
    );
    // Other tools should not be denied
    assert_eq!(
        check_permission(&config, "Read", &json!({})),
        PermissionResult::Allow
    );
}

#[test]
fn test_allow_rules() {
    let config = PermissionConfig {
        mode: PermissionMode::Default,
        allow_rules: vec![Rule {
            tool_name: "Read".to_string(),
            pattern: None,
        }],
        ..Default::default()
    };

    assert_eq!(
        check_permission(&config, "Read", &json!({})),
        PermissionResult::Allow
    );
}

#[test]
fn test_wildcard_deny() {
    let config = PermissionConfig {
        mode: PermissionMode::Default,
        deny_rules: vec![Rule {
            tool_name: "mcp__*".to_string(),
            pattern: None,
        }],
        allow_rules: vec![Rule {
            tool_name: "*".to_string(),
            pattern: None,
        }],
        ..Default::default()
    };

    assert_eq!(
        check_permission(&config, "mcp__server__tool", &json!({})),
        PermissionResult::Deny("Tool 'mcp__server__tool' denied by rule".to_string())
    );
    assert_eq!(
        check_permission(&config, "Read", &json!({})),
        PermissionResult::Allow
    );
}

#[test]
fn test_allowed_tools_whitelist() {
    let config = PermissionConfig {
        mode: PermissionMode::Default,
        allowed_tools: Some(vec!["Read".to_string(), "Glob".to_string()]),
        ..Default::default()
    };

    assert_eq!(
        check_permission(&config, "Read", &json!({})),
        PermissionResult::Allow
    );
    match check_permission(&config, "Bash", &json!({})) {
        PermissionResult::Deny(_) => {}
        _ => panic!("Expected deny"),
    }
}

#[test]
fn test_deny_with_pattern() {
    let config = PermissionConfig {
        mode: PermissionMode::Default,
        deny_rules: vec![Rule {
            tool_name: "Bash".to_string(),
            pattern: Some("rm".to_string()),
        }],
        allow_rules: vec![Rule {
            tool_name: "*".to_string(),
            pattern: None,
        }],
        ..Default::default()
    };

    // Deny when pattern matches
    assert_eq!(
        check_permission(&config, "Bash", &json!({"command": "rm file.txt"})),
        PermissionResult::Deny("Tool 'Bash' denied by rule".to_string())
    );

    // Allow when pattern doesn't match
    assert_eq!(
        check_permission(&config, "Bash", &json!({"command": "ls"})),
        PermissionResult::Allow
    );
}

// --- Filesystem Validator Tests ---

#[test]
fn test_is_sensitive_path() {
    assert!(FilesystemValidator::is_sensitive_path("/home/user/.env"));
    assert!(FilesystemValidator::is_sensitive_path("/home/user/.env.local"));
    assert!(FilesystemValidator::is_sensitive_path(
        "/home/user/.ssh/id_rsa"
    ));
    assert!(FilesystemValidator::is_sensitive_path(
        "/app/credentials.json"
    ));
    assert!(!FilesystemValidator::is_sensitive_path("/app/main.rs"));
    assert!(!FilesystemValidator::is_sensitive_path(
        "/app/src/config.rs"
    ));
}

#[test]
fn test_filesystem_validator() {
    let validator = FilesystemValidator::new("/tmp/project");

    // Paths within working dir should be allowed
    assert!(validator.validate_path("/tmp/project/src/main.rs", false).is_ok());
    assert!(validator.validate_path("/tmp/project/src/main.rs", true).is_ok());
}

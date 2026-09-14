use open_agent_sdk::costtracker::CostTracker;
use open_agent_sdk::types::Usage;

#[tokio::test]
async fn test_cost_tracker_new() {
    let tracker = CostTracker::new();
    assert_eq!(tracker.total_cost().await, 0.0);
    assert_eq!(tracker.total_tokens().await, 0);
}

#[tokio::test]
async fn test_cost_tracker_add_usage() {
    let tracker = CostTracker::new();

    let usage = Usage {
        input_tokens: 1000,
        output_tokens: 500,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };

    tracker.add_usage("claude-sonnet-4-6-20250514", &usage).await;

    assert_eq!(tracker.total_tokens().await, 1500);
    assert!(tracker.total_cost().await > 0.0);
}

#[tokio::test]
async fn test_cost_tracker_multiple_models() {
    let tracker = CostTracker::new();

    tracker
        .add_usage(
            "claude-sonnet-4-6-20250514",
            &Usage {
                input_tokens: 1000,
                output_tokens: 500,
                ..Default::default()
            },
        )
        .await;

    tracker
        .add_usage(
            "claude-opus-4-6-20250514",
            &Usage {
                input_tokens: 500,
                output_tokens: 200,
                ..Default::default()
            },
        )
        .await;

    assert_eq!(tracker.total_tokens().await, 2200);

    let sonnet_usage = tracker
        .get_model_usage("claude-sonnet-4-6-20250514")
        .await;
    assert!(sonnet_usage.is_some());
    assert_eq!(sonnet_usage.unwrap().input_tokens, 1000);

    let opus_usage = tracker
        .get_model_usage("claude-opus-4-6-20250514")
        .await;
    assert!(opus_usage.is_some());
    assert_eq!(opus_usage.unwrap().input_tokens, 500);
}

#[tokio::test]
async fn test_cost_tracker_accumulates() {
    let tracker = CostTracker::new();

    let usage = Usage {
        input_tokens: 1000,
        output_tokens: 500,
        ..Default::default()
    };

    tracker.add_usage("claude-sonnet-4-6-20250514", &usage).await;
    tracker.add_usage("claude-sonnet-4-6-20250514", &usage).await;

    assert_eq!(tracker.total_tokens().await, 3000);

    let model_usage = tracker
        .get_model_usage("claude-sonnet-4-6-20250514")
        .await
        .unwrap();
    assert_eq!(model_usage.input_tokens, 2000);
    assert_eq!(model_usage.output_tokens, 1000);
}

#[tokio::test]
async fn test_cost_tracker_durations() {
    let tracker = CostTracker::new();

    tracker.add_api_duration(100).await;
    tracker.add_api_duration(200).await;
    tracker.add_tool_duration(50).await;

    let summary = tracker.summary().await;
    assert_eq!(summary.api_duration_ms, 300);
    assert_eq!(summary.tool_duration_ms, 50);
}

#[tokio::test]
async fn test_cost_tracker_code_changes() {
    let tracker = CostTracker::new();

    tracker.add_code_changes(10, 5).await;
    tracker.add_code_changes(20, 3).await;

    let summary = tracker.summary().await;
    assert_eq!(summary.lines_added, 30);
    assert_eq!(summary.lines_removed, 8);
}

#[tokio::test]
async fn test_cost_tracker_web_searches() {
    let tracker = CostTracker::new();

    tracker.add_web_search().await;
    tracker.add_web_search().await;

    let summary = tracker.summary().await;
    assert_eq!(summary.web_searches, 2);
}

#[tokio::test]
async fn test_cost_calculation_sonnet() {
    let tracker = CostTracker::new();

    // 1M input + 1M output for sonnet = $3 + $15 = $18
    tracker
        .add_usage(
            "claude-sonnet-4-6-20250514",
            &Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                ..Default::default()
            },
        )
        .await;

    let cost = tracker.total_cost().await;
    assert!((cost - 18.0).abs() < 0.01);
}

#[tokio::test]
async fn test_cost_calculation_opus() {
    let tracker = CostTracker::new();

    // 1M input + 1M output for opus = $15 + $75 = $90
    tracker
        .add_usage(
            "claude-opus-4-6-20250514",
            &Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                ..Default::default()
            },
        )
        .await;

    let cost = tracker.total_cost().await;
    assert!((cost - 90.0).abs() < 0.01);
}

#[tokio::test]
async fn test_get_all_usage() {
    let tracker = CostTracker::new();

    tracker
        .add_usage(
            "model-a",
            &Usage {
                input_tokens: 100,
                ..Default::default()
            },
        )
        .await;
    tracker
        .add_usage(
            "model-b",
            &Usage {
                input_tokens: 200,
                ..Default::default()
            },
        )
        .await;

    let all = tracker.get_all_usage().await;
    assert_eq!(all.len(), 2);
    assert!(all.contains_key("model-a"));
    assert!(all.contains_key("model-b"));
}

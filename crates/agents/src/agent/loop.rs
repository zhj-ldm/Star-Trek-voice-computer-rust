use crate::api::ApiRouter;
use crate::context;
use crate::costtracker::CostTracker;
use crate::tools::{self, ToolRegistry};
use crate::types::*;
use crate::utils::{compact, messages as msg_utils, retry};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Run the main agentic loop.
pub(crate) async fn run_loop(
    api_client: ApiRouter,
    mut messages: Vec<Message>,
    registry: Arc<ToolRegistry>,
    cwd: &str,
    system_prompt: Option<&str>,
    append_system_prompt: Option<&str>,
    max_turns: u32,
    max_budget_usd: Option<f64>,
    cost_tracker: &CostTracker,
    thinking: Option<ThinkingConfig>,
    max_tokens: Option<u64>,
    can_use_tool: Option<&CanUseToolFn>,
    tx: mpsc::Sender<SDKMessage>,
    cancel: CancellationToken,
) -> Result<Vec<Message>, String> {
    let tool_context = ToolUseContext::new(cwd.to_string());

    // Build system prompt blocks
    let system_blocks = context::build_system_blocks(cwd, system_prompt, append_system_prompt);

    // Build tool definitions for API
    let api_tools: Vec<ApiToolParam> = registry
        .all()
        .iter()
        .map(|tool| {
            let schema = tool.input_schema();
            ApiToolParam {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: serde_json::to_value(&schema).unwrap_or_default(),
            }
        })
        .collect();

    let mut num_turns: u32 = 0;
    let mut total_usage = Usage::default();

    // Send system message
    let _ = tx
        .send(SDKMessage::System {
            message: format!(
                "Agent started with {} tools ({})",
                api_tools.len(),
                match api_client.api_type() {
                    crate::api::ApiType::AnthropicMessages => "anthropic",
                    crate::api::ApiType::OpenAICompletions => "openai",
                }
            ),
        })
        .await;

    loop {
        // Check interrupt / abort signal (语音打断 / 用户主动中断)
        if cancel.is_cancelled() {
            let _ = tx
                .send(SDKMessage::Progress {
                    message: "Interrupted".to_string(),
                })
                .await;
            break;
        }

        // Check turn limit
        num_turns += 1;
        if num_turns > max_turns {
            let _ = tx
                .send(SDKMessage::Progress {
                    message: format!("Reached max turns ({})", max_turns),
                })
                .await;
            break;
        }

        // Check budget
        if let Some(budget) = max_budget_usd {
            let current_cost = cost_tracker.total_cost().await;
            if current_cost >= budget {
                let _ = tx
                    .send(SDKMessage::Progress {
                        message: format!(
                            "Budget exceeded: ${:.4} >= ${:.4}",
                            current_cost, budget
                        ),
                    })
                    .await;
                break;
            }
        }

        // Check if auto-compaction is needed
        if compact::should_auto_compact(&messages, &api_client.model()) {
            messages = compact::micro_compact_messages(&messages);
        }

        // Normalize messages
        let normalized = msg_utils::normalize_messages(&messages);

        // Call the API via streaming provider abstraction.
        // 轮询路由：每"一次逻辑请求"只推进一次 round-robin 位置。
        let client = api_client.next();
        let system_blocks_ref = &system_blocks;
        let api_tools_ref = &api_tools;
        let thinking_ref = &thinking;

        let start = std::time::Instant::now();

        // Stream the response, forwarding text & reasoning deltas to the
        // frontend while assembling the full ProviderResponse for tool use.
        let stream_result = client
            .create_message_stream(
                &normalized,
                Some(system_blocks_ref.clone()),
                Some(api_tools_ref.clone()),
                max_tokens,
                thinking_ref.clone(),
            )
            .await;

        let mut rx = match stream_result {
            Ok(rx) => rx,
            Err(e) => {
                if retry::is_auth_error(&e) {
                    return Err(format!("Authentication error: {}. Check your API key.", e));
                }
                return Err(format!("API error: {}", e));
            }
        };

        let mut text_content = String::new();
        let mut thinking_content = String::new();
        let mut tool_calls: std::collections::HashMap<usize, (String, String, String)> =
            std::collections::HashMap::new();
        let mut usage = Usage::default();
        let mut stop_reason: Option<String> = None;
        let mut stream_cancelled = false;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    stream_cancelled = true;
                    break;
                }
                maybe_chunk = rx.recv() => {
                    match maybe_chunk {
                        None => break, // stream closed
                        Some(chunk) => {
                            match chunk {
                                Ok(crate::api::StreamChunk::TextDelta(t)) => {
                                    text_content.push_str(&t);
                                    let _ = tx
                                        .send(SDKMessage::PartialMessage { text: t })
                                        .await;
                                }
                                Ok(crate::api::StreamChunk::ThinkingDelta(t)) => {
                                    thinking_content.push_str(&t);
                                    let _ = tx
                                        .send(SDKMessage::ReasoningDelta { text: t })
                                        .await;
                                }
                                Ok(crate::api::StreamChunk::ToolUseStart { index, id, name }) => {
                                    let entry = tool_calls
                                        .entry(index)
                                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                                    if !id.is_empty() { entry.0 = id; }
                                    if !name.is_empty() { entry.1 = name; }
                                }
                                Ok(crate::api::StreamChunk::ToolUseDelta { index, partial_json }) => {
                                    let entry = tool_calls
                                        .entry(index)
                                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                                    entry.2.push_str(&partial_json);
                                }
                                Ok(crate::api::StreamChunk::Usage(u)) => usage = u,
                                Ok(crate::api::StreamChunk::StopReason(sr)) => stop_reason = Some(sr),
                                Ok(crate::api::StreamChunk::Done) => break,
                                Err(e) => {
                                    return Err(format!("API stream error: {}", e));
                                }
                            }
                        }
                    }
                }
            }
        }

        if stream_cancelled {
            let _ = tx
                .send(SDKMessage::Progress {
                    message: "Interrupted during streaming".to_string(),
                })
                .await;
            break;
        }

        // Assemble the full assistant message from streamed chunks.
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        if !thinking_content.is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                thinking: thinking_content,
                signature: None,
            });
        }
        if !text_content.is_empty() {
            content_blocks.push(ContentBlock::Text {
                text: text_content,
            });
        }
        let mut sorted_calls: Vec<(usize, (String, String, String))> =
            tool_calls.into_iter().collect();
        sorted_calls.sort_by_key(|(idx, _)| *idx);
        for (_, (id, name, args)) in sorted_calls {
            let input: serde_json::Value = serde_json::from_str(&args)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
            content_blocks.push(ContentBlock::ToolUse {
                id: if id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4())
                } else {
                    id
                },
                name,
                input,
            });
        }

        let provider_response = crate::api::ProviderResponse {
            message: Message {
                role: MessageRole::Assistant,
                content: content_blocks,
            },
            usage: usage.clone(),
            stop_reason,
        };

        let api_duration = start.elapsed().as_millis() as u64;
        cost_tracker.add_api_duration(api_duration).await;
        cost_tracker
            .add_usage(&api_client.model(), &provider_response.usage)
            .await;

        let usage = &provider_response.usage;
        let assistant_msg = provider_response.message;
        let stop_reason = provider_response.stop_reason;

        // Accumulate usage
        total_usage.input_tokens += usage.input_tokens;
        total_usage.output_tokens += usage.output_tokens;
        total_usage.cache_creation_input_tokens += usage.cache_creation_input_tokens;
        total_usage.cache_read_input_tokens += usage.cache_read_input_tokens;

        // Send assistant message event
        let _ = tx
            .send(SDKMessage::Assistant {
                message: assistant_msg.clone(),
                usage: Some(usage.clone()),
            })
            .await;

        messages.push(assistant_msg.clone());

        // Check for tool use
        let tool_uses = extract_tool_uses(&assistant_msg);

        if tool_uses.is_empty() {
            // No tool calls - we're done (end_turn or max_tokens)
            break;
        }

        // Execute tools
        let tool_start = std::time::Instant::now();
        // 工具执行前再检查一次 cancel 信号（流式到工具之间的窗口期）
        if cancel.is_cancelled() {
            break;
        }
        let results =
            tools::execute_tools(&assistant_msg, &registry, &tool_context, can_use_tool, Some(cancel.clone())).await;
        let tool_duration = tool_start.elapsed().as_millis() as u64;
        cost_tracker.add_tool_duration(tool_duration).await;

        // Send tool result events
        for (id, name, result) in &results {
            let content_text = result.get_text();
            let _ = tx
                .send(SDKMessage::ToolResult {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    content: content_text,
                    is_error: result.is_error,
                })
                .await;
        }

        // Build tool results message and add to conversation
        let tool_results_msg = tools::build_tool_results_message(&results);
        messages.push(tool_results_msg);

        // Check stop reason
        if stop_reason.as_deref() == Some("end_turn") && tool_uses.is_empty() {
            break;
        }
    }

    // Calculate final cost
    let total_cost = cost_tracker.total_cost().await;

    // Extract final text
    let final_text = messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant)
        .map(|m| extract_text(m))
        .unwrap_or_default();

    // Send result event
    let _ = tx
        .send(SDKMessage::Result {
            text: final_text,
            usage: total_usage,
            num_turns,
            cost_usd: total_cost,
            duration_ms: 0,
            messages: messages.clone(),
        })
        .await;

    Ok(messages)
}

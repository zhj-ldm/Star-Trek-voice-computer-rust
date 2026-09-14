use crate::types::{ContentBlock, Message, MessageRole};

/// Create a user message with text content.
pub fn create_user_message(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

/// Create an assistant message with text content.
pub fn create_assistant_message(text: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

/// Ensure messages alternate between user and assistant roles.
/// Merges consecutive messages with the same role.
pub fn normalize_messages(messages: &[Message]) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }

    let mut normalized = Vec::new();

    for msg in messages {
        if let Some(last) = normalized.last_mut() {
            let last: &mut Message = last;
            if last.role == msg.role {
                // Merge content blocks
                last.content.extend(msg.content.clone());
                continue;
            }
        }
        normalized.push(msg.clone());
    }

    // Ensure first message is from user
    if !normalized.is_empty() && normalized[0].role != MessageRole::User {
        normalized.insert(
            0,
            Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "(continuing conversation)".to_string(),
                }],
            },
        );
    }

    normalized
}

/// Extract text from a message's content blocks.
pub fn extract_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Strip image content blocks from messages (for compaction).
pub fn strip_images(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|msg| {
            let content: Vec<ContentBlock> = msg
                .content
                .iter()
                .filter(|block| !matches!(block, ContentBlock::Image { .. }))
                .cloned()
                .collect();
            Message {
                role: msg.role.clone(),
                content,
            }
        })
        .collect()
}

/// Truncate text to a maximum length.
pub fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}... (truncated)", &text[..max_len])
    }
}

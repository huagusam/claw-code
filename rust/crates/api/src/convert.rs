use std::collections::HashMap;
use std::sync::Arc;

use runtime::image_store::ImageStore;
use runtime::{ContentBlock, ConversationMessage, MessageRole};

use crate::types::ImageSource;
use crate::{InputContentBlock, InputMessage, ToolResultContentBlock};

use serde_json::Value;

/// Core conversion logic.  Returns plain `Vec` (no `Arc` wrapper) so callers
/// that maintain their own accumulator can append delta conversions without
/// an intermediate `Arc` allocation.
///
/// Delta messages (assistant replies, tool results) never contain `ImageRef`
/// blocks, so callers may pass `None` for both `image_cache` and `image_store`
/// when converting a slice that is known to contain no user-originated messages.
pub fn convert_messages_inner(
    messages: &[ConversationMessage],
    image_cache: Option<&HashMap<String, String>>,
    image_store: Option<&ImageStore>,
) -> (Vec<InputMessage>, Vec<Option<Value>>) {
    let mut input_messages = Vec::with_capacity(messages.len());
    let mut cached_values = Vec::with_capacity(messages.len());

    for message in messages {
        let role = match message.role {
            MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
            MessageRole::Assistant => "assistant",
        };
        let content: Vec<InputContentBlock> = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Thinking { .. } => None,
                ContentBlock::Text { text } => {
                    Some(InputContentBlock::Text { text: text.clone() })
                }
                ContentBlock::ToolUse { id, name, input } => {
                    Some(InputContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    })
                }
                ContentBlock::Image {
                    mime_type, data, ..
                } => Some(InputContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".to_string(),
                        media_type: mime_type.clone(),
                        data: data.clone(),
                    },
                }),
                ContentBlock::ImageRef { hash_hex, mime_type, .. } => {
                    let base64_data = image_cache
                        .and_then(|cache| cache.get(hash_hex))
                        .cloned()
                        .or_else(|| {
                            image_store
                                .and_then(|store| store.load_base64(hash_hex, mime_type).ok())
                        })
                        .unwrap_or_default();
                    if base64_data.is_empty() {
                        eprintln!(
                            "[IMAGE] Failed to resolve base64 for hash {hash_hex} (mime: {mime_type})"
                        );
                    }
                    Some(InputContentBlock::Image {
                        source: ImageSource {
                            source_type: "base64".to_string(),
                            media_type: mime_type.clone(),
                            data: base64_data,
                        },
                    })
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    output,
                    is_error,
                    ..
                } => Some(InputContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: vec![ToolResultContentBlock::Text {
                        text: output.clone(),
                    }],
                    is_error: *is_error,
                }),
            })
            .collect();

        if content.is_empty() {
            // Message has no non-Thinking content (e.g. only Thinking blocks
            // that were stripped above).  Include a placeholder text block so
            // the message count stays aligned with `cached_message_values` —
            // dropping it here would make `cached_values` shorter than the
            // original message list, corrupting the IncrementalBody per-message
            // byte cache used by `send_raw_request`.
            let input_msg = InputMessage {
                role: role.to_string(),
                content: vec![InputContentBlock::Text {
                    text: String::new(),
                }],
            };
            cached_values.push(None);
            input_messages.push(input_msg);
            continue;
        }

        let input_msg = InputMessage {
            role: role.to_string(),
            content,
        };

        let cached = message
            .cached_input_message
            .get_or_init(|| serde_json::to_value(&input_msg).unwrap_or(Value::Null));

        cached_values.push(Some(cached.clone()));
        input_messages.push(input_msg);
    }

    (input_messages, cached_values)
}

/// Convert the runtime-level `ConversationMessage` list into the
/// API-level `InputMessage` list suitable for Anthropic / OpenAI requests.
///
/// * Thinking blocks are dropped.
/// * `ImageRef` blocks are resolved to base64 via `image_cache` / `image_store`.
/// * Returns `Arc<Vec<InputMessage>>` so callers can cheaply share the
///   result across clones (e.g. in `MessageRequest`).
#[must_use]
pub fn convert_messages(
    messages: &[ConversationMessage],
    image_cache: Option<&HashMap<String, String>>,
    image_store: Option<&ImageStore>,
) -> Arc<Vec<InputMessage>> {
    Arc::new(convert_messages_inner(messages, image_cache, image_store).0)
}

/// Like `convert_messages` but also returns cached serialised JSON `Value`s
/// for each converted message.
///
/// The cached values are stored in `ConversationMessage.cached_input_message`
/// on the first call and reused on subsequent calls within the same
/// `filter_for_api` batch.  Callers that use `IncrementalBody` should prefer
/// this variant so the body builder can skip re-serialising unchanged messages.
#[must_use]
pub fn convert_messages_cached(
    messages: &[ConversationMessage],
    image_cache: Option<&HashMap<String, String>>,
    image_store: Option<&ImageStore>,
) -> (Arc<Vec<InputMessage>>, Vec<Option<Value>>) {
    let (msgs, vals) = convert_messages_inner(messages, image_cache, image_store);
    (Arc::new(msgs), vals)
}

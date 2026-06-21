use runtime::{pricing_for_model, TokenUsage, UsageCostEstimate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Anthropic extended thinking configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub config_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MessageRequest {
    pub model: String,
    pub max_tokens: u32,
    /// Shared message list wrapped in `Arc` so that `MessageRequest::clone()`
    /// is O(1) for the (typically large) messages vector.
    pub messages: Arc<Vec<InputMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    /// OpenAI-compatible tuning parameters. Optional — omitted from payload when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    /// Reasoning effort level for OpenAI-compatible reasoning models (e.g. `o4-mini`).
    /// Accepted values: `"low"`, `"medium"`, `"high"`. Omitted when `None`.
    /// Silently ignored by backends that do not support it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Anthropic extended thinking configuration. Omitted when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// Pre-cached serialised JSON `Value`s for each message, typically produced
    /// by `convert_messages_cached`.  The `IncrementalBody` will use these to
    /// skip re-serialisation of unchanged messages.
    /// Empty when not using the cache.
    /// Wrapped in `Arc` so that `MessageRequest::clone()` is O(1).
    #[serde(skip)]
    pub cached_message_values: Arc<Vec<Option<Value>>>,
}

impl MessageRequest {
    #[must_use]
    pub fn with_streaming(mut self) -> Self {
        self.stream = true;
        self
    }

    /// Render the request body in Anthropic API JSON format.
    ///
    /// Post-processing steps:
    /// 1. Normalize image blocks to Anthropic's nested `source` format.
    /// 2. Split system prompt at `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` into blocks
    ///    with `cache_control: ephemeral` on the static portion.
    /// 3. Add `cache_control: ephemeral` to the last tool definition.
    #[inline]
    pub fn render_anthropic_body(&self) -> Result<Value, serde_json::Error> {
        let mut body = serde_json::to_value(self)?;
        Self::apply_system_prompt_cache_control(&mut body);
        Self::apply_tools_cache_control(&mut body);
        Ok(body)
    }

    /// Split the flat system prompt string at `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`
    /// into Anthropic's block format with `cache_control` on the static part.
    ///
    /// Before: `"system": "static...\n\n__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__\n\ndynamic..."`
    /// After:  `"system": [{"type":"text","text":"static...","cache_control":{"type":"ephemeral"}},
    ///                      {"type":"text","text":"dynamic..."}]`
    pub(crate) fn apply_system_prompt_cache_control(body: &mut Value) {
        let Some(system_str) = body
            .get("system")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
        else {
            return;
        };
        let boundary = runtime::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
        let Some(split_pos) = system_str.find(boundary) else {
            // No boundary marker — wrap entire system as cached
            if !system_str.is_empty() {
                body["system"] = serde_json::json!([{
                    "type": "text",
                    "text": system_str,
                    "cache_control": { "type": "ephemeral" }
                }]);
            }
            return;
        };
        let static_part = system_str[..split_pos].trim_end().to_string();
        let dynamic_part = system_str[split_pos + boundary.len()..]
            .trim_start()
            .to_string();
        let mut blocks = Vec::new();
        if !static_part.is_empty() {
            blocks.push(serde_json::json!({
                "type": "text",
                "text": static_part,
                "cache_control": { "type": "ephemeral" }
            }));
        }
        if !dynamic_part.is_empty() {
            blocks.push(serde_json::json!({
                "type": "text",
                "text": dynamic_part
            }));
        }
        if !blocks.is_empty() {
            body["system"] = Value::Array(blocks);
        }
    }

    /// Add `cache_control: ephemeral` to the last tool definition so Anthropic
    /// caches the tool schema across requests within the same turn.
    pub(crate) fn apply_tools_cache_control(body: &mut Value) {
        let Some(tools) = body
            .get_mut("tools")
            .and_then(|v| v.as_array_mut())
        else {
            return;
        };
        if let Some(last_tool) = tools.last_mut() {
            if let Some(obj) = last_tool.as_object_mut() {
                obj.insert(
                    "cache_control".to_string(),
                    serde_json::json!({ "type": "ephemeral" }),
                );
            }
        }
    }

}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputMessage {
    pub role: String,
    pub content: Vec<InputContentBlock>,
}

impl InputMessage {
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text { text: text.into() }],
        }
    }

    #[must_use]
    pub fn user_tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![InputContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: vec![ToolResultContentBlock::Text {
                    text: content.into(),
                }],
                is_error,
            }],
        }
    }
}

/// Nested source block for Anthropic's `{"type":"image","source":{...}}` format.
///
/// Serde serialises this directly into the shape that Anthropic's API expects,
/// eliminating the need for a post-processing pass that walks the entire
/// body tree looking for `Image` blocks to normalise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageSource {
    /// Always `"base64"`.
    #[serde(rename = "type")]
    pub source_type: String,
    /// MIME type of the image (e.g. `"image/png"`, `"image/jpeg"`).
    pub media_type: String,
    /// Base64-encoded image data.
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultContentBlock>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    Image {
        /// Nested `source` block in Anthropic's expected format, produced
        /// directly at construction time so no JSON-level post-processing
        /// is needed.
        #[serde(rename = "source")]
        source: ImageSource,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContentBlock {
    Text { text: String },
    Json { value: Value },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: Vec<OutputContentBlock>,
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl MessageResponse {
    #[must_use]
    pub fn total_tokens(&self) -> u32 {
        self.usage.total_tokens()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default = "serde_json::Value::default")]
        input: Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: Value,
    },
    // Added image output block
    Image {
        data: String,
        mime_type: String,
        filename: Option<String>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

impl Usage {
    #[must_use]
    pub const fn total_tokens(&self) -> u32 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }

    #[must_use]
    pub const fn token_usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
        }
    }

    #[must_use]
    pub fn estimated_cost_usd(&self, model: &str) -> UsageCostEstimate {
        let usage = self.token_usage();
        pricing_for_model(model).map_or_else(
            || usage.estimate_cost_usd(),
            |pricing| usage.estimate_cost_usd_with_pricing(pricing),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStartEvent {
    pub message: MessageResponse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaEvent {
    pub delta: MessageDelta,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDelta {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockStartEvent {
    pub index: u32,
    pub content_block: OutputContentBlock,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockDeltaEvent {
    pub index: u32,
    pub delta: ContentBlockDelta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentBlockStopEvent {
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageStopEvent {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart(MessageStartEvent),
    MessageDelta(MessageDeltaEvent),
    ContentBlockStart(ContentBlockStartEvent),
    ContentBlockDelta(ContentBlockDeltaEvent),
    ContentBlockStop(ContentBlockStopEvent),
    MessageStop(MessageStopEvent),
}

#[cfg(test)]
mod tests {
    use runtime::format_usd;

    use super::{MessageResponse, Usage};

    #[test]
    fn usage_total_tokens_includes_cache_tokens() {
        let usage = Usage {
            input_tokens: 10,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 3,
            output_tokens: 4,
        };

        assert_eq!(usage.total_tokens(), 19);
        assert_eq!(usage.token_usage().total_tokens(), 19);
    }

    #[test]
    fn message_response_estimates_cost_from_model_usage() {
        let response = MessageResponse {
            id: "msg_cost".to_string(),
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: Vec::new(),
            model: "claude-sonnet-4-20250514".to_string(),
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: Usage {
                input_tokens: 1_000_000,
                cache_creation_input_tokens: 100_000,
                cache_read_input_tokens: 200_000,
                output_tokens: 500_000,
            },
            request_id: None,
        };

        let cost = response.usage.estimated_cost_usd(&response.model);
        assert_eq!(format_usd(cost.total_cost_usd()), "$54.6750");
        assert_eq!(response.total_tokens(), 1_800_000);
    }
}
